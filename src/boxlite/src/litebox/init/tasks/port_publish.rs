//! Task: publish configured host ports through the running network backend.
//!
//! The backend owns endpoint allocation. This task owns lifecycle policy:
//! fixed endpoints are published before automatic ones, partial publication is
//! rolled back, and concrete bindings are returned in request order.
//!
//! The task never has to wait for the shim's runtime port control. The shim
//! creates gvproxy before it boots the VM, and `gvproxy_create` returns only
//! once the ServicesMux control socket is listening — so by the time any plan
//! reaches this task, the socket has been serving since before the guest ran
//! its first instruction. Nothing downstream reads the bindings either, so
//! publication runs alongside container init rather than gating it.

use super::{InitCtx, log_task_error, task_start};
use crate::litebox::ports::LivePublishedPorts;
use crate::net::constants::GUEST_IP;
use crate::net::{Forward, NetworkBackend, TransportProtocol};
use crate::pipeline::PipelineTask;
use crate::runtime::options::PortSpec;
use crate::runtime::types::PublishedPort;
use crate::util::{PidFileReader, ShimPidRecord};
use async_trait::async_trait;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

/// The endpoints a planned mapping may adopt from an already-running backend.
///
/// gvproxy keys forwards by `protocol/local`, so a fixed request can only ever
/// match its own endpoint, while automatic requests are interchangeable within
/// their guest endpoint.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum MatchKey {
    Fixed(SocketAddr),
    Automatic(IpAddr, SocketAddr, TransportProtocol),
}

/// One publication request resolved against the guest, keeping the caller's
/// original position so results can be reported in request order.
struct PlannedPort {
    request_index: usize,
    guest_port: u16,
    protocol: crate::runtime::options::PortProtocol,
    local: SocketAddr,
    remote: SocketAddr,
    transport: TransportProtocol,
}

impl PlannedPort {
    /// Resolve every request up front. Validating the whole plan before the
    /// first backend call keeps a bad later mapping from stranding earlier
    /// forwards.
    fn plan(requested: &[PortSpec]) -> BoxliteResult<Vec<Self>> {
        let guest_ip = GUEST_IP.parse::<IpAddr>().map_err(|error| {
            BoxliteError::Internal(format!("invalid built-in guest IP {GUEST_IP}: {error}"))
        })?;

        requested
            .iter()
            .enumerate()
            .map(|(request_index, request)| {
                Ok(Self {
                    request_index,
                    guest_port: request.guest_port,
                    protocol: request.protocol,
                    local: request.validate_publishable()?,
                    remote: SocketAddr::new(guest_ip, request.guest_port),
                    transport: request.protocol.into(),
                })
            })
            .collect()
    }

    fn is_automatic(&self) -> bool {
        self.local.port() == 0
    }

    fn match_key(&self) -> MatchKey {
        if self.is_automatic() {
            MatchKey::Automatic(self.local.ip(), self.remote, self.transport)
        } else {
            MatchKey::Fixed(self.local)
        }
    }

    fn published_at(&self, local: SocketAddr) -> PublishedPort {
        PublishedPort {
            guest_port: self.guest_port,
            host_ip: local.ip().to_string(),
            host_port: local.port(),
            protocol: self.protocol,
        }
    }
}

/// Forwards the backend already serves, which a reattaching publisher adopts
/// instead of standing up a second listener beside them.
///
/// Matching is a multiset assignment: equivalent automatic requests take the
/// matching endpoints in ascending port order, so repeated reattachments of one
/// configuration always resolve the same way.
#[derive(Default)]
struct AdoptableForwards {
    /// Live endpoints keyed by the guest endpoint they serve. A forward the
    /// backend reports in a form we cannot match — unparseable, unknown
    /// protocol, or still unresolved — is simply not adoptable.
    unclaimed: Vec<(MatchKey, SocketAddr)>,
}

impl AdoptableForwards {
    fn new(active: &[Forward]) -> Self {
        Self {
            unclaimed: active.iter().filter_map(Self::adoptable_endpoint).collect(),
        }
    }

    fn adoptable_endpoint(forward: &Forward) -> Option<(MatchKey, SocketAddr)> {
        let local = forward.local.parse::<SocketAddr>().ok()?;
        let remote = forward.remote.parse::<SocketAddr>().ok()?;
        let transport = TransportProtocol::from_wire(&forward.protocol)?;
        (local.port() != 0).then_some((MatchKey::Automatic(local.ip(), remote, transport), local))
    }

    /// Claim the endpoint already serving `mapping`, if there is one.
    ///
    /// `equivalent_requests` is how many mappings in the plan could adopt the
    /// same endpoints; more live matches than that means the backend holds
    /// forwards this configuration cannot account for, and guessing which one
    /// belongs to which request would silently orphan a listener.
    fn claim(
        &mut self,
        mapping: &PlannedPort,
        equivalent_requests: usize,
    ) -> BoxliteResult<Option<SocketAddr>> {
        let guest_endpoint =
            MatchKey::Automatic(mapping.local.ip(), mapping.remote, mapping.transport);
        let mut matches = self
            .unclaimed
            .iter()
            .enumerate()
            .filter(|(_, (key, local))| {
                *key == guest_endpoint && (mapping.is_automatic() || *local == mapping.local)
            })
            .map(|(index, (_, local))| (*local, index))
            .collect::<Vec<_>>();
        if matches.len() > equivalent_requests {
            return Err(BoxliteError::InvalidState(format!(
                "configured port {} matches multiple active backend forwards",
                mapping.guest_port
            )));
        }

        matches.sort_unstable();
        let Some((local, index)) = matches.first().copied() else {
            return Ok(None);
        };
        self.unclaimed.remove(index);
        Ok(Some(local))
    }
}

/// Publishes a box's configured ports through its one network backend.
///
/// Both entry points wait for the shim's runtime port control before the first
/// mutation, then give every planned mapping a concrete endpoint. A failure
/// part-way through rolls back whatever this publisher itself published.
struct PortPublisher<'a> {
    backend: &'a dyn NetworkBackend,
    /// The plan in publication order — fixed endpoints first, so an automatic
    /// allocation can never take a host port another mapping asked for.
    planned: Vec<PlannedPort>,
    /// Forwards published by this run, in publication order: the rollback
    /// ledger. Adopted forwards are deliberately absent — they belong to the
    /// shim, not to this attempt.
    published: Vec<Forward>,
}

impl<'a> PortPublisher<'a> {
    fn new(backend: &'a dyn NetworkBackend, mut planned: Vec<PlannedPort>) -> Self {
        // Stable sort, so requests keep their relative order within each group.
        planned.sort_by_key(PlannedPort::is_automatic);
        Self {
            backend,
            planned,
            published: Vec::new(),
        }
    }

    /// Publish every configured mapping on a freshly spawned shim.
    ///
    /// A new shim owns no forwards, so there is nothing to adopt.
    async fn publish(mut self) -> BoxliteResult<Vec<PublishedPort>> {
        self.assign(AdoptableForwards::default()).await
    }

    /// Adopt the forwards a running shim already owns and publish the rest.
    ///
    /// Reattach repairs publication after a core-process restart, so it has to
    /// be idempotent: a mapping the backend already serves is claimed, never
    /// duplicated.
    async fn reconcile(mut self) -> BoxliteResult<Vec<PublishedPort>> {
        let active = self.backend.list_forwards().await?;
        self.assign(AdoptableForwards::new(&active)).await
    }

    async fn assign(
        &mut self,
        mut adoptable: AdoptableForwards,
    ) -> BoxliteResult<Vec<PublishedPort>> {
        let planned = std::mem::take(&mut self.planned);
        let mut equivalent_requests: HashMap<MatchKey, usize> = HashMap::new();
        for mapping in &planned {
            *equivalent_requests.entry(mapping.match_key()).or_default() += 1;
        }

        let mut resolved = Vec::with_capacity(planned.len());
        for mapping in &planned {
            let equivalents = equivalent_requests[&mapping.match_key()];
            match self
                .endpoint_for(mapping, &mut adoptable, equivalents)
                .await
            {
                Ok(local) => resolved.push((mapping.request_index, mapping.published_at(local))),
                Err(error) => {
                    self.rollback().await;
                    return Err(error);
                }
            }
        }

        resolved.sort_by_key(|(request_index, _)| *request_index);
        Ok(resolved.into_iter().map(|(_, port)| port).collect())
    }

    /// Adopt this mapping's live endpoint, or publish a new forward for it.
    async fn endpoint_for(
        &mut self,
        mapping: &PlannedPort,
        adoptable: &mut AdoptableForwards,
        equivalent_requests: usize,
    ) -> BoxliteResult<SocketAddr> {
        if let Some(local) = adoptable.claim(mapping, equivalent_requests)? {
            return Ok(local);
        }

        let forward = self
            .backend
            .expose(
                &mapping.local.to_string(),
                &mapping.remote.to_string(),
                mapping.transport,
            )
            .await?;
        self.published.push(forward.clone());
        forward.local.parse::<SocketAddr>().map_err(|error| {
            BoxliteError::Network(format!(
                "network backend returned invalid local endpoint {:?}: {error}",
                forward.local
            ))
        })
    }

    /// Undo this run's own publications, newest first. Best effort: the caller
    /// is already returning the failure that triggered the rollback.
    async fn rollback(&self) {
        for forward in self.published.iter().rev() {
            let Some(protocol) = TransportProtocol::from_wire(&forward.protocol) else {
                tracing::warn!(
                    local = %forward.local,
                    protocol = %forward.protocol,
                    "Cannot roll back port publication with unknown protocol"
                );
                continue;
            };
            if let Err(error) = self.backend.unexpose(&forward.local, protocol).await {
                tracing::warn!(
                    local = %forward.local,
                    %error,
                    "Failed to roll back port publication"
                );
            }
        }
    }
}

pub struct PortPublishTask;

impl PortPublishTask {
    /// Publish `requested` through a shim that has just been spawned.
    pub(crate) async fn publish(
        backend: Option<&dyn NetworkBackend>,
        requested: &[PortSpec],
        lifecycle: ShimPidRecord,
    ) -> BoxliteResult<Vec<PublishedPort>> {
        match Self::publisher(backend, requested, lifecycle)? {
            Some(publisher) => publisher.publish().await,
            None => Ok(Vec::new()),
        }
    }

    /// Repair publication for an already-active lifecycle.
    ///
    /// The reattach pipeline uses this entry point to adopt or restore forwards
    /// after a core-process restart. `None` means the bindings stay unresolved.
    /// Metadata reads never call this mutating path; they report only what the
    /// live state already knows.
    pub(crate) async fn reconcile(
        backend: Option<&dyn NetworkBackend>,
        requested: &[PortSpec],
        lifecycle: ShimPidRecord,
    ) -> BoxliteResult<Option<Vec<PublishedPort>>> {
        // Older shims own custom listeners that are not visible through
        // ServicesMux. Never risk creating a second listener beside them; the
        // bindings remain explicitly unresolved until a normal restart.
        if !lifecycle.has_runtime_port_control() {
            tracing::warn!(
                "Legacy shim has no runtime port control; leaving its listeners untouched"
            );
            return Ok(None);
        }

        match Self::publisher(backend, requested, lifecycle)? {
            Some(publisher) => publisher.reconcile().await.map(Some),
            None => Ok(Some(Vec::new())),
        }
    }

    /// Build a publisher for a non-empty request, or `None` when there is
    /// nothing to publish and therefore no backend to require.
    fn publisher<'a>(
        backend: Option<&'a dyn NetworkBackend>,
        requested: &[PortSpec],
        lifecycle: ShimPidRecord,
    ) -> BoxliteResult<Option<PortPublisher<'a>>> {
        let planned = PlannedPort::plan(requested)?;
        if planned.is_empty() {
            return Ok(None);
        }

        if !lifecycle.has_runtime_port_control() {
            return Err(BoxliteError::Internal(
                "new shim PID record does not advertise ServicesMux runtime port control"
                    .to_string(),
            ));
        }

        let backend = backend.ok_or_else(|| {
            BoxliteError::Unsupported(
                "host port publication requires an active network backend".to_string(),
            )
        })?;
        Ok(Some(PortPublisher::new(backend, planned)))
    }
}

/// Which pipeline is running the task. The two differ in more than one place,
/// so they travel as a named mode rather than a bare flag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PublicationMode {
    /// Start or restart: this task brings the box's listeners into existence.
    FreshStart,
    /// Reattach: the shim is already running and may already own listeners.
    Reattach,
}

impl PublicationMode {
    /// Read the shim lifecycle that owns the listeners, then publish or repair
    /// against that exact lifecycle.
    ///
    /// `None` means there are no live bindings to remember: either nothing was
    /// requested, or a legacy shim's listeners were left alone.
    async fn run(
        self,
        backend: Option<&dyn NetworkBackend>,
        requested: &[PortSpec],
        pid_path: PathBuf,
    ) -> BoxliteResult<Option<LivePublishedPorts>> {
        // Nothing to publish: touch neither the shim's identity nor the
        // backend. `BoxInfo` derives the empty set from configuration alone,
        // so a box without ports needs nothing from this task.
        if requested.is_empty() {
            return Ok(None);
        }

        let lifecycle = PidFileReader::at(pid_path).read_shim()?;
        let published = match self {
            Self::FreshStart => {
                Some(PortPublishTask::publish(backend, requested, lifecycle).await?)
            }
            Self::Reattach => PortPublishTask::reconcile(backend, requested, lifecycle).await?,
        };
        Ok(published.map(|ports| LivePublishedPorts::new(lifecycle.identity(), ports)))
    }

    /// Decide the task's result from a publication outcome.
    ///
    /// Reconciliation is repair work on an already-running shim, so a transient
    /// control-plane error must not leave its `CleanupGuard` armed and tear
    /// down the healthy workload.
    fn finish(
        self,
        box_id: &crate::BoxID,
        task_name: &str,
        result: BoxliteResult<()>,
    ) -> BoxliteResult<()> {
        match result {
            Ok(()) => Ok(()),
            Err(error) if self == Self::Reattach => {
                tracing::warn!(
                    box_id = %box_id,
                    %error,
                    "Port publication reconciliation failed; preserving the running box"
                );
                Ok(())
            }
            Err(error) => {
                log_task_error(box_id, task_name, &error);
                Err(error)
            }
        }
    }
}

#[async_trait]
impl PipelineTask<InitCtx> for PortPublishTask {
    async fn run(self: Box<Self>, ctx: InitCtx) -> BoxliteResult<()> {
        let task_name = self.name();
        let box_id = task_start(&ctx, task_name).await;

        let (requested, pid_path, backend, mode) = {
            let mut ctx = ctx.lock().await;
            let layout = ctx
                .layout
                .as_ref()
                .ok_or_else(|| BoxliteError::Internal("filesystem task must run first".into()))?;
            (
                ctx.config.options.ports.clone(),
                layout.pid_file_path(),
                ctx.network_backend.take(),
                // The reattach plan is the one that skips the guest-ready wait.
                if ctx.skip_guest_wait {
                    PublicationMode::Reattach
                } else {
                    PublicationMode::FreshStart
                },
            )
        };

        let outcome = mode.run(backend.as_deref(), &requested, pid_path).await;

        // The backend must not stay behind the pipeline mutex while control
        // requests await the shim, so ownership returns here on every path.
        {
            let mut ctx = ctx.lock().await;
            ctx.network_backend = backend;
            if let Ok(Some(published_ports)) = &outcome {
                ctx.published_ports = Some(published_ports.clone());
            }
        }

        mode.finish(&box_id, task_name, outcome.map(|_| ()))
    }

    fn name(&self) -> &str {
        "port_publish"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{NetworkBackendSpec, TransportProtocol};
    use crate::runtime::options::PortProtocol;
    use crate::util::PidRecord;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::Mutex;

    #[derive(Debug)]
    enum ExposeResult {
        Bound(&'static str),
        Failed(&'static str),
    }

    #[derive(Debug, Default)]
    struct MockBackend {
        results: Mutex<VecDeque<ExposeResult>>,
        active: Mutex<Vec<Forward>>,
        exposed: Mutex<Vec<String>>,
        unexposed: Mutex<Vec<String>>,
        list_error: Mutex<Option<&'static str>>,
        list_calls: Mutex<usize>,
    }

    impl MockBackend {
        fn new(results: impl IntoIterator<Item = ExposeResult>) -> Self {
            Self {
                results: Mutex::new(results.into_iter().collect()),
                ..Default::default()
            }
        }

        fn with_active(self, active: Vec<Forward>) -> Self {
            *self.active.lock().unwrap() = active;
            self
        }

        fn with_list_error(self, message: &'static str) -> Self {
            *self.list_error.lock().unwrap() = Some(message);
            self
        }

        fn list_call_count(&self) -> usize {
            *self.list_calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl NetworkBackend for MockBackend {
        fn name(&self) -> &'static str {
            "mock"
        }

        fn spec(&self) -> NetworkBackendSpec {
            NetworkBackendSpec {
                socket_path: PathBuf::from("/tmp/mock-net.sock"),
                allow_net: Vec::new(),
                secrets: Vec::new(),
                ca_cert_pem: None,
                ca_key_pem: None,
                net_bandwidth: Default::default(),
            }
        }

        async fn expose(
            &self,
            local: &str,
            remote: &str,
            protocol: TransportProtocol,
        ) -> BoxliteResult<Forward> {
            self.exposed.lock().unwrap().push(local.to_string());
            let bound = match self.results.lock().unwrap().pop_front().unwrap() {
                ExposeResult::Bound(bound) => bound,
                ExposeResult::Failed(message) => {
                    return Err(BoxliteError::Network(message.to_string()));
                }
            };
            let forward = Forward {
                local: bound.to_string(),
                remote: remote.to_string(),
                protocol: protocol.as_str().to_string(),
            };
            self.active.lock().unwrap().push(forward.clone());
            Ok(forward)
        }

        async fn unexpose(&self, local: &str, _protocol: TransportProtocol) -> BoxliteResult<()> {
            self.unexposed.lock().unwrap().push(local.to_string());
            self.active
                .lock()
                .unwrap()
                .retain(|forward| forward.local != local);
            Ok(())
        }

        async fn list_forwards(&self) -> BoxliteResult<Vec<Forward>> {
            *self.list_calls.lock().unwrap() += 1;
            if let Some(message) = *self.list_error.lock().unwrap() {
                return Err(BoxliteError::Network(message.to_string()));
            }
            Ok(self.active.lock().unwrap().clone())
        }
    }

    fn mapping(host_port: Option<u16>, guest_port: u16) -> PortSpec {
        PortSpec {
            host_port,
            guest_port,
            protocol: PortProtocol::Tcp,
            host_ip: Some("127.0.0.1".to_string()),
        }
    }

    fn published_port(host_port: u16, guest_port: u16) -> PublishedPort {
        PublishedPort {
            guest_port,
            host_ip: "127.0.0.1".to_string(),
            host_port,
            protocol: PortProtocol::Tcp,
        }
    }

    fn active_forward(local: &str, guest_port: u16) -> Forward {
        Forward {
            local: local.to_string(),
            remote: format!("{GUEST_IP}:{guest_port}"),
            protocol: "tcp".to_string(),
        }
    }

    fn lifecycle(pid: u32, start_time: u64) -> ShimPidRecord {
        ShimPidRecord::with_runtime_port_control(PidRecord {
            pid,
            start_time: Some(start_time),
        })
    }

    /// Build the publisher the task would build, with a test-sized readiness
    /// wait. Mirrors [`PortPublishTask::publisher`] without the pipeline.
    fn publisher<'a>(
        backend: &'a MockBackend,
        requested: &[PortSpec],
    ) -> BoxliteResult<PortPublisher<'a>> {
        Ok(PortPublisher::new(backend, PlannedPort::plan(requested)?))
    }

    async fn publish(
        backend: &MockBackend,
        requested: &[PortSpec],
    ) -> BoxliteResult<Vec<PublishedPort>> {
        publisher(backend, requested)?.publish().await
    }

    async fn reconcile(
        backend: &MockBackend,
        requested: &[PortSpec],
    ) -> BoxliteResult<Vec<PublishedPort>> {
        publisher(backend, requested)?.reconcile().await
    }

    #[tokio::test]
    async fn publishes_fixed_first_and_returns_request_order() {
        let backend = MockBackend::new([
            ExposeResult::Bound("127.0.0.1:18080"),
            ExposeResult::Bound("127.0.0.1:49152"),
        ]);
        let requested = vec![mapping(None, 3000), mapping(Some(18080), 8080)];

        let resolved = publish(&backend, &requested).await.unwrap();

        assert_eq!(
            *backend.exposed.lock().unwrap(),
            vec!["127.0.0.1:18080", "127.0.0.1:0"],
            "a fixed host port must be claimed before an automatic allocation could take it"
        );
        assert_eq!(
            resolved,
            vec![published_port(49152, 3000), published_port(18080, 8080)]
        );
    }

    #[tokio::test]
    async fn later_failure_rolls_back_successful_forwards_in_reverse() {
        let backend = MockBackend::new([
            ExposeResult::Bound("127.0.0.1:18080"),
            ExposeResult::Bound("127.0.0.1:18081"),
            ExposeResult::Failed("third publication failed"),
        ]);
        let requested = vec![
            mapping(Some(18080), 80),
            mapping(Some(18081), 81),
            mapping(Some(18082), 82),
        ];

        let error = publish(&backend, &requested).await.unwrap_err();

        assert!(error.to_string().contains("third publication failed"));
        assert_eq!(
            *backend.unexposed.lock().unwrap(),
            vec!["127.0.0.1:18081", "127.0.0.1:18080"]
        );
    }

    #[tokio::test]
    async fn validates_complete_plan_before_exposing_any_forward() {
        let backend = MockBackend::new([ExposeResult::Bound("127.0.0.1:18080")]);
        let requested = vec![mapping(Some(18080), 80), mapping(Some(18081), 0)];

        let error = publish(&backend, &requested).await.unwrap_err();

        assert!(error.to_string().contains("guest port"));
        assert!(backend.exposed.lock().unwrap().is_empty());
        assert!(backend.unexposed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn publishes_duplicate_automatic_mappings() {
        let backend = MockBackend::new([
            ExposeResult::Bound("127.0.0.1:49152"),
            ExposeResult::Bound("127.0.0.1:49153"),
        ]);
        let requested = vec![mapping(None, 3000), mapping(Some(0), 3000)];

        let resolved = publish(&backend, &requested).await.unwrap();

        assert_eq!(
            resolved,
            vec![published_port(49152, 3000), published_port(49153, 3000)]
        );
        assert_eq!(
            *backend.exposed.lock().unwrap(),
            vec!["127.0.0.1:0", "127.0.0.1:0"]
        );
        assert_eq!(
            backend.list_call_count(),
            0,
            "a fresh publication has nothing to adopt and never lists forwards"
        );
    }

    #[tokio::test]
    async fn reattach_recovers_duplicate_automatic_mappings_deterministically() {
        let backend = MockBackend::default().with_active(vec![
            active_forward("127.0.0.1:49153", 3000),
            active_forward("127.0.0.1:49152", 3000),
        ]);
        let requested = vec![mapping(None, 3000), mapping(Some(0), 3000)];

        let resolved = reconcile(&backend, &requested).await.unwrap();

        assert_eq!(
            resolved,
            vec![published_port(49152, 3000), published_port(49153, 3000)],
            "interchangeable requests take matching endpoints in ascending port order"
        );
        assert!(backend.exposed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn reattach_adopts_existing_fixed_forward_and_publishes_only_missing_auto() {
        let backend = MockBackend::new([ExposeResult::Bound("127.0.0.1:49152")])
            .with_active(vec![active_forward("127.0.0.1:18080", 8080)]);
        let requested = vec![mapping(None, 3000), mapping(Some(18080), 8080)];

        let resolved = reconcile(&backend, &requested).await.unwrap();

        assert_eq!(*backend.exposed.lock().unwrap(), vec!["127.0.0.1:0"]);
        assert_eq!(
            resolved,
            vec![published_port(49152, 3000), published_port(18080, 8080)]
        );
        assert!(backend.unexposed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn reattach_recovers_automatic_forward_from_live_backend() {
        let backend =
            MockBackend::default().with_active(vec![active_forward("127.0.0.1:49152", 3000)]);

        let resolved = reconcile(&backend, &[mapping(None, 3000)]).await.unwrap();

        assert_eq!(resolved, vec![published_port(49152, 3000)]);
        assert!(backend.exposed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn reattach_after_prepublication_crash_publishes_missing_forward() {
        let backend = MockBackend::new([ExposeResult::Bound("127.0.0.1:49152")]);

        let resolved = reconcile(&backend, &[mapping(None, 3000)]).await.unwrap();

        assert_eq!(*backend.exposed.lock().unwrap(), vec!["127.0.0.1:0"]);
        assert_eq!(resolved, vec![published_port(49152, 3000)]);
    }

    #[tokio::test]
    async fn reattach_ambiguity_rolls_back_newly_published_forwards() {
        let original = vec![
            active_forward("127.0.0.1:49152", 3000),
            active_forward("127.0.0.1:49153", 3000),
        ];
        let backend = MockBackend::new([ExposeResult::Bound("127.0.0.1:18080")])
            .with_active(original.clone());
        let requested = vec![mapping(None, 3000), mapping(Some(18080), 8080)];

        let error = reconcile(&backend, &requested).await.unwrap_err();

        assert!(
            error
                .to_string()
                .contains("multiple active backend forwards")
        );
        assert_eq!(*backend.unexposed.lock().unwrap(), vec!["127.0.0.1:18080"]);
        assert_eq!(
            *backend.active.lock().unwrap(),
            original,
            "adopted forwards belong to the shim and must survive a rollback"
        );
    }

    #[tokio::test]
    async fn reattach_list_failure_is_reported_without_mutation() {
        let backend = MockBackend::default().with_list_error("gvproxy control socket unavailable");

        let error = reconcile(&backend, &[mapping(None, 3000)])
            .await
            .unwrap_err();

        assert!(error.to_string().contains("control socket unavailable"));
        assert!(backend.exposed.lock().unwrap().is_empty());
        assert!(backend.unexposed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn successful_publication_keeps_forward_active() {
        let backend = MockBackend::new([ExposeResult::Bound("127.0.0.1:18080")]);

        let resolved = publish(&backend, &[mapping(Some(18080), 80)])
            .await
            .unwrap();

        assert_eq!(resolved, vec![published_port(18080, 80)]);
        assert_eq!(
            *backend.active.lock().unwrap(),
            vec![active_forward("127.0.0.1:18080", 80)]
        );
        assert!(backend.unexposed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn empty_plan_does_not_require_backend() {
        let resolved = PortPublishTask::publish(None, &[], lifecycle(108, 1008))
            .await
            .unwrap();

        assert!(resolved.is_empty());
    }

    #[tokio::test]
    async fn nonempty_plan_requires_backend() {
        let error = PortPublishTask::publish(None, &[mapping(None, 3000)], lifecycle(109, 1009))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("active network backend"));
    }

    #[tokio::test]
    async fn legacy_reattach_leaves_listeners_untouched_and_unresolved() {
        let requested = vec![mapping(None, 3000), mapping(Some(18080), 8080)];
        let backend = MockBackend::new([ExposeResult::Bound("127.0.0.1:49153")]);
        let legacy = ShimPidRecord::legacy(PidRecord {
            pid: 110,
            start_time: Some(1010),
        });

        let resolved = PortPublishTask::reconcile(Some(&backend), &requested, legacy)
            .await
            .unwrap();

        assert!(resolved.is_none());
        assert!(backend.exposed.lock().unwrap().is_empty());
        assert_eq!(
            backend.list_call_count(),
            0,
            "a legacy shim is never probed"
        );
    }

    #[test]
    fn reattach_publication_error_does_not_fail_initialization() {
        let box_id = crate::BoxID::parse("port-reconcile-test").unwrap();
        let failure = || {
            Err(BoxliteError::Network(
                "gvproxy control socket unavailable".to_string(),
            ))
        };

        assert!(
            PublicationMode::Reattach
                .finish(&box_id, "port_publish", failure())
                .is_ok(),
            "repair work on a running shim must not arm its CleanupGuard"
        );
        assert!(
            PublicationMode::FreshStart
                .finish(&box_id, "port_publish", failure())
                .is_err(),
            "a box that never came up must still fail its pipeline"
        );
    }

    /// A box that publishes nothing must not depend on the shim's identity
    /// file at all — most boxes have no ports, and reading it would couple
    /// every start to a file this task has no use for.
    #[tokio::test]
    async fn no_requested_ports_never_reads_the_shim_identity() {
        let absent = PathBuf::from("/nonexistent/boxlite-no-such-box/shim.pid");

        for mode in [PublicationMode::FreshStart, PublicationMode::Reattach] {
            let published = mode
                .run(None, &[], absent.clone())
                .await
                .expect("a box without ports must start");

            assert!(
                published.is_none(),
                "{mode:?} must record no live bindings for a box without ports"
            );
        }
    }

    #[tokio::test]
    async fn fresh_publication_rejects_a_legacy_shim() {
        let backend = MockBackend::new([ExposeResult::Bound("127.0.0.1:49152")]);
        let legacy = ShimPidRecord::legacy(PidRecord {
            pid: 111,
            start_time: Some(1011),
        });

        let error = PortPublishTask::publish(Some(&backend), &[mapping(None, 3000)], legacy)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("runtime port control"));
        assert!(backend.exposed.lock().unwrap().is_empty());
    }
}
