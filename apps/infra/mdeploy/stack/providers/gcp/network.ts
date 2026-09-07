/*
 * The GCP network: one VPC, a Cloud Router with NAT, and one service account
 * per role.
 *
 * The service accounts are where this differs most from AWS, and the difference
 * is the deepest one between the two clouds. AWS answers *who may reach this*
 * with position — a security group says which tasks reach the API, and being in
 * it is the whole of the permission. Google answers with identity: a Cloud Run
 * service admits one named invoker, a firewall rule keys on a service account,
 * and a database grants an account. So a placement here carries an identity
 * where the AWS one carries a group, and this module is where those identities
 * are created — because a role's identity has to exist before anything that
 * grants to it does.
 *
 * Private Service Access is the other thing with no AWS counterpart. Cloud SQL
 * and Memorystore are Google-managed and live in Google's own project; reaching
 * them on a private address needs a peering between this network and that one,
 * with a reserved range for it. Nothing can take a private address until the
 * peering exists, so it is in `ready`.
 *
 * The runner's placement is `egress-only-public` here as on AWS, and means the
 * same thing by different means: the instance gets an external address so its
 * image pulls do not queue behind Cloud NAT, and the only ingress rule that
 * names its service account admits the subnet's own range.
 */

import { API_PORT } from '../../api.ts'
import { OTLP_HTTP_PORT } from '../../collector.ts'
import { PROXY_PORT } from '../../edge.ts'
import type { Network, NetworkProvider, NetworkRequest, Placement, WorkloadRole } from '../../network.ts'
import { RUNNER_PORT } from '../../runners.ts'

/** Every port one workload in this network opens to another. */
const INTERNAL_PORTS = [API_PORT, PROXY_PORT, RUNNER_PORT, OTLP_HTTP_PORT].map(String)

/** The subnet workloads sit in. Private Service Access gets its own below. */
const SUBNET_CIDR = '10.20.0.0/20'

/**
 * The range reserved for Google's own managed services.
 *
 * A `/16` because Google allocates out of it per service and per region, and a
 * range too small to allocate from fails at the point a database is created
 * rather than here — with an error about the service, not about the range.
 */
const SERVICE_RANGE_PREFIX = 16

/** The four identities, one per role. A grant names one of these and never a range. */
const ACCOUNTS: Record<WorkloadRole, string> = {
  api: 'api',
  proxy: 'proxy',
  'otel-collector': 'otel',
  runner: 'runner',
}

export const gcpNetworkProvider =
  ({ project, region }: { project: string; region: string }): NetworkProvider =>
  (request: NetworkRequest): Network => {
    const prefix = `${$app.name}-${$app.stage}`

    const network = new gcp.compute.Network('Network', {
      name: prefix,
      project,
      // Subnets are declared rather than generated: one per stage, in one
      // region, is the whole topology — and an auto-created subnet in every
      // region is twenty-odd ranges nothing uses.
      autoCreateSubnetworks: false,
    })

    const subnetwork = new gcp.compute.Subnetwork('Subnetwork', {
      name: prefix,
      project,
      region,
      network: network.id,
      ipCidrRange: SUBNET_CIDR,
      // Cloud Run reaches this subnet through a connector or direct VPC egress;
      // both require Google's own access to the range.
      privateIpGoogleAccess: true,
    })

    /*
     * Outbound internet for workloads with no address of their own.
     *
     * A router and a NAT rather than a NAT instance: Google has no equivalent
     * of the cheap EC2 NAT the AWS side runs, and Cloud NAT is the managed
     * answer. `internetEgress: false` builds neither, which is a stage whose
     * workloads cannot pull an image — supported, and named rather than assumed.
     */
    const router = request.internetEgress
      ? new gcp.compute.Router('Router', { name: prefix, project, region, network: network.id })
      : null
    const nat = router
      ? new gcp.compute.RouterNat('Nat', {
          name: prefix,
          project,
          region,
          router: router.name,
          natIpAllocateOption: 'AUTO_ONLY',
          sourceSubnetworkIpRangesToNat: 'ALL_SUBNETWORKS_ALL_IP_RANGES',
          logConfig: { enable: true, filter: 'ERRORS_ONLY' },
        })
      : null

    /*
     * The peering that lets a managed database take a private address.
     *
     * Two resources for one idea: a range reserved out of this network, and the
     * connection that hands it to Google's service producer. Both, and in this
     * order, or Cloud SQL refuses a private IP with an error about the service.
     */
    const serviceRange = new gcp.compute.GlobalAddress('PrivateServiceRange', {
      name: `${prefix}-psa`,
      project,
      purpose: 'VPC_PEERING',
      addressType: 'INTERNAL',
      prefixLength: SERVICE_RANGE_PREFIX,
      network: network.id,
    })
    const privateServiceAccess = new gcp.servicenetworking.Connection('PrivateServiceAccess', {
      network: network.id,
      service: 'servicenetworking.googleapis.com',
      reservedPeeringRanges: [serviceRange.name],
    })

    const accounts = Object.fromEntries(
      Object.entries(ACCOUNTS).map(([role, id]) => [
        role,
        new gcp.serviceaccount.Account(`${id.charAt(0).toUpperCase()}${id.slice(1)}ServiceAccount`, {
          project,
          accountId: `${prefix}-${id}`.slice(0, 30),
          displayName: `BoxLite ${role} (${$app.stage})`,
        }),
      ]),
    ) as Record<WorkloadRole, CloudResource>

    /*
     * Service to service, keyed on identity rather than on a range.
     *
     * `sourceServiceAccounts` is the property that makes this the mirror of the
     * AWS security-group pair: a workload is admitted because of who it is, not
     * because of where it sits. A rule keyed on the subnet range would also
     * admit anything else that ever lands in it.
     */
    const serviceIdentities = [accounts.api, accounts.proxy, accounts['otel-collector']].map((account) => account.email)
    const internal = new gcp.compute.Firewall('InternalFirewall', {
      name: `${prefix}-internal`,
      project,
      network: network.id,
      direction: 'INGRESS',
      priority: 1000,
      allows: [{ protocol: 'tcp', ports: INTERNAL_PORTS }],
      sourceServiceAccounts: serviceIdentities,
      targetServiceAccounts: serviceIdentities,
    })

    // The runner answers the API and the proxy, and nothing else reaches it.
    const runnerIngress = new gcp.compute.Firewall('RunnerFirewall', {
      name: `${prefix}-runner`,
      project,
      network: network.id,
      direction: 'INGRESS',
      priority: 1000,
      allows: [{ protocol: 'tcp', ports: [String(RUNNER_PORT)] }],
      sourceServiceAccounts: [accounts.api.email, accounts.proxy.email],
      targetServiceAccounts: [accounts.runner.email],
    })

    // And the other direction: a runner registers itself and ships telemetry.
    const runnerEgress = new gcp.compute.Firewall('RunnerToServicesFirewall', {
      name: `${prefix}-runner-to-services`,
      project,
      network: network.id,
      direction: 'INGRESS',
      priority: 1000,
      allows: [{ protocol: 'tcp', ports: [String(API_PORT), String(OTLP_HTTP_PORT)] }],
      sourceServiceAccounts: [accounts.runner.email],
      targetServiceAccounts: [accounts.api.email, accounts['otel-collector'].email],
    })

    /*
     * Deny everything else inbound.
     *
     * Google's implied rules already deny ingress, but only at the lowest
     * priority — a rule someone adds later at a higher one silently wins. An
     * explicit deny at 65534 makes any such rule a visible edit to this file's
     * neighbourhood rather than an invisible widening.
     */
    const denied = new gcp.compute.Firewall('DenyIngressFirewall', {
      name: `${prefix}-deny-ingress`,
      project,
      network: network.id,
      direction: 'INGRESS',
      priority: 65534,
      denies: [{ protocol: 'all' }],
      sourceRanges: ['0.0.0.0/0'],
    })

    const placementFor = (role: WorkloadRole): Placement => ({
      cloud: 'gcp',
      exposure: role === 'runner' ? 'egress-only-public' : 'private',
      subnetwork: subnetwork.id,
      serviceAccount: accounts[role].email,
    })

    return {
      binding: {
        cloud: 'gcp',
        network: network.selfLink,
        subnetwork: subnetwork.selfLink,
        privateServiceAccess: privateServiceAccess.id,
        cidr: subnetwork.ipCidrRange,
      },
      placementFor,
      ready: [
        privateServiceAccess,
        internal,
        runnerIngress,
        runnerEgress,
        denied,
        ...(nat ? [nat] : []),
      ],
    }
  }
