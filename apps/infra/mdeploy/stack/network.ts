/*
 * The private network everything else runs inside.
 *
 * This is the least portable of the modules, and the contract is shaped by
 * admitting that rather than by hiding it. What the two clouds genuinely agree
 * on is small: workloads run somewhere without a public address, they can reach
 * the internet outbound, and some of them may reach each other on a named port.
 * Everything below that — subnets, security groups, firewall rules, service
 * networking peerings — is one cloud's answer and not the other's.
 *
 * So the portable surface is `placementFor(role)`: what a workload is handed so
 * that it lands in the network with the reachability its role is supposed to
 * have. A caller attaches a placement and never learns whether the reachability
 * behind it came from a security group pair or a firewall rule keyed on a
 * service account.
 *
 * BoxLite has one role the others do not, and it is the reason `exposure` is on
 * the placement rather than implied by it. The runner is a virtual machine with
 * nested KVM, and it sits in a *public* subnet with an egress-only public IP:
 * it egresses through the internet gateway rather than the NAT the services
 * use, and nothing on the internet can reach it, because its one inbound rule
 * admits only the network itself. That is a deliberate placement and not an
 * oversight, so the contract names it — a provider that quietly gave the runner
 * the services' private placement would work until the first image pull.
 *
 * Roles are a closed set rather than free-form strings. There are four of them
 * and a graph engine to express four edges would be more machinery than the
 * edges. A fifth role is a deliberate edit here, which is the right cost.
 */

/** The four kinds of workload BoxLite runs. */
export type WorkloadRole = 'api' | 'proxy' | 'otel-collector' | 'runner'

/**
 * Where in the network a role sits.
 *
 * `private` is a workload with no address of its own, reaching the internet
 * through a NAT. `egress-only-public` is BoxLite's runner: an address it dials
 * out from, and no route in. The distinction is the network's to make, because
 * both clouds express it in the network rather than at the workload.
 */
export type Exposure = 'private' | 'egress-only-public'

export type NetworkRequest = {
  /** Outbound internet for private workloads. Off means no NAT and no egress. */
  internetEgress: boolean
}

/**
 * What a workload is given so it runs inside the network with its role's
 * reachability. Attached at the workload; never referenced by the network.
 */
export type Placement =
  | {
      cloud: 'aws'
      exposure: Exposure
      /** Private for a service, public for the runner. See `exposure`. */
      subnets: $util.Output<string[]>
      /** The security groups this role's reachability is expressed through. */
      securityGroups: $util.Output<string>[]
    }
  | {
      cloud: 'gcp'
      exposure: Exposure
      /** The subnetwork a Cloud Run service egresses through, or a VM sits in. */
      subnetwork: $util.Output<string>
      /** The identity firewall rules name, and that IAM grants attach to. */
      serviceAccount: $util.Output<string>
    }

/**
 * The cloud-specific half, for the modules that genuinely need the network
 * itself rather than a placement in it: the database and the cache need
 * somewhere to put a private address, the load balancers need the public
 * subnets, and ClickHouse needs both.
 */
export type NetworkBinding =
  | {
      cloud: 'aws'
      vpc: CloudResource
      vpcId: $util.Output<string>
      privateSubnets: $util.Output<string[]>
      /** Only the load balancers and the runner belong here. */
      publicSubnets: $util.Output<string[]>
      cloudmapNamespaceId: $util.Output<string>
      cloudmapNamespaceName: $util.Output<string>
      /** The CIDR a runner's one inbound rule admits, and nothing wider. */
      cidr: $util.Output<string>
    }
  | {
      cloud: 'gcp'
      /** Self link, which is what Cloud SQL and the peering both want. */
      network: $util.Output<string>
      subnetwork: $util.Output<string>
      /**
       * The Private Service Access peering. Cloud SQL cannot take a private
       * address until it exists, so it is also in `ready`.
       */
      privateServiceAccess: $util.Output<string>
      cidr: $util.Output<string>
    }

export type Network = {
  binding: NetworkBinding
  placementFor: (role: WorkloadRole) => Placement
  ready: any[]
}

export type NetworkProvider = (request: NetworkRequest) => Network
