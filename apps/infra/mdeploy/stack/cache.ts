/*
 * The transient cache the control plane keeps its queues and sessions in.
 *
 * Shaped like the database and deliberately smaller. There is no backup
 * retention here and no protection flag, because there is nothing in it worth
 * keeping: a cache that is lost is refilled, which is the property that lets a
 * stage be torn down without a snapshot.
 *
 * The one thing worth saying twice is that the password is a reference on both
 * clouds and a value on neither. ElastiCache and Memorystore both mint one, and
 * both put it somewhere a workload is granted rather than handed — so nothing
 * about the shape here changes between them even though the resources do.
 */

export type CacheSize = 'small' | 'medium'

export type CacheRequest = {
  size: CacheSize
  /** More than one node. Off is one node, which is what a cache usually wants. */
  clustered: boolean
  /** Refuse plaintext on the wire. On everywhere; here to be refused loudly. */
  encryptInTransit: boolean
}

export type CacheConnection = {
  host: $util.Output<string>
  port: $util.Output<string>
}

export type CacheBinding =
  | { cloud: 'aws'; passwordRef: $util.Output<string>; clientGrant: $util.Output<string> }
  | { cloud: 'gcp'; passwordRef: $util.Output<string>; clientGrant: $util.Output<string> }

export type Cache = {
  connection: CacheConnection
  binding: CacheBinding
  id: $util.Output<string>
  ready: any[]
}

export type CacheProvider = (request: CacheRequest) => Cache

/** The name the API reads the cache password under (`apps/api` configuration.ts). */
export const CACHE_PASSWORD_VARIABLE = 'REDIS_PASSWORD'

/**
 * The environment a workload reads to reach this cache.
 *
 * `REDIS_TLS` is decided here rather than by the caller: `encryptInTransit` is
 * a request the provider has already had to honour, and a client told to speak
 * plaintext to a server that refuses it fails at the first command with an
 * error about the protocol rather than about the setting.
 */
export const cacheEnvironment = (cache: Cache): Record<string, $util.Output<string> | string> => ({
  REDIS_HOST: cache.connection.host,
  REDIS_PORT: cache.connection.port,
  REDIS_TLS: 'true',
})
