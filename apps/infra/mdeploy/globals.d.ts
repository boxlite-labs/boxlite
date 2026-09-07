/*
 * The names both engines inject, declared so mdeploy can be typechecked.
 *
 * SST generates `.sst/platform/config.d.ts` when it installs, and that file is
 * a build artefact: it is not in the repository, and a `tsc` run that has not
 * had `sst install` first cannot see any of these. The Pulumi engine does not
 * generate one at all — `pulumi/program.ts` defines the same names itself.
 * Between the two, a stack written against globals had nowhere to get its
 * types from, which is why the repository this pattern came from left its own
 * mdeploy outside the typecheck entirely.
 *
 * This file closes that. It declares what the stack actually reads, and no
 * more: `$util`'s `Input` and `Output` are given real shapes, because the
 * contracts are written in terms of them and getting those wrong is exactly the
 * mistake worth catching. The provider namespaces are `any`, which is the
 * honest limit — checking a raw `gcp.compute.Instance` argument needs
 * `@pulumi/gcp`'s own types, and importing those here would make every AWS
 * stage carry a package it never loads.
 *
 * So `npm run typecheck:mdeploy` proves the wiring: that every module's request
 * and handle agree, that a provider returns what its contract promises, and
 * that a capability the contract added has been expanded. It does not prove a
 * resource argument's spelling. That is a smaller guarantee than SST's own
 * types would give and a much larger one than none.
 */

declare global {
  /** The app and stage this deploy is for. Both engines set it. */
  const $app: { readonly name: string; readonly stage: string }

  namespace $util {
    /** A value that may or may not be resolved yet. */
    type Input<T> = T | Output<T> | Promise<T>

    /**
     * A value the engine resolves during an apply.
     *
     * The numeric index is not decoration: an `Output<string[]>` is indexable in
     * both engines — `vpc.privateSubnets[0]` is how a provider names one subnet
     * — and a declaration without it would make every such line an error here
     * while working perfectly at deploy time.
     */
    interface Output<T> {
      apply<U>(callback: (value: T) => U): Output<U extends Output<infer V> ? V : U>
      readonly [index: number]: Output<any>
      readonly __output: T
    }
  }

  /** Wraps a plain value, and marks one as secret so the state seals it. */
  const $util: {
    output<T>(value: $util.Input<T>): $util.Output<T>
    secret<T>(value: $util.Input<T>): $util.Output<T>
    interpolate(strings: TemplateStringsArray, ...values: any[]): $util.Output<string>
    jsonStringify(value: any): $util.Output<string>
    all(values: any[]): $util.Output<any[]>
  }

  /** Template-literal interpolation over values that are not resolved yet. */
  function $interpolate(strings: TemplateStringsArray, ...values: any[]): $util.Output<string>

  function $output<T>(value: $util.Input<T>): $util.Output<T>
  function $jsonStringify(value: any): $util.Output<string>

  /** Waits for several values at once. `$util.all` under the name the stack uses. */
  function $resolve<T extends any[]>(values: T): $util.Output<{ [K in keyof T]: any }>

  /**
   * SST's raw-resource hook. Declared because `sst.config.ts` uses it, and
   * deliberately absent from the Pulumi engine's globals — its only caller is
   * the AWS role-boundary rule, and a GCP resource reaching for it should fail
   * loudly rather than silently do nothing.
   */
  function $transform(type: any, callback: (args: any, options?: any, name?: string) => void): void

  /** Paths the CLI resolved. Only the ClickHouse reconcile command reads it. */
  const $cli: { paths: { root: string } }

  /**
   * The provider namespaces, as `any`.
   *
   * See the note at the top: giving these real types means importing three
   * large packages into every typecheck, including in a repository whose stages
   * are all on one cloud. What this file guarantees is the wiring between
   * modules, not the spelling of a resource's arguments.
   */
  const aws: any
  const sst: any
  const gcp: any
  const random: any
  const cloudflare: any
  const command: any

  /**
   * A resource handle a contract holds on to but never reads into.
   *
   * The AWS member of `WorkloadHost` carries an `sst.aws.Cluster`, and two
   * providers keep a secret and its version around to name in a `dependsOn`.
   * Those are real types with real shapes, and this file cannot see them —
   * naming that here beats spelling `any` at each site, where it would read as
   * carelessness rather than as the one thing this typecheck does not cover.
   */
  type CloudResource = any
}

export {}
