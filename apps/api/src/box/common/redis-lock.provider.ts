/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { InjectRedis } from '@nestjs-modules/ioredis'
import { Injectable, Logger } from '@nestjs/common'
import { Redis } from 'ioredis'
import { randomUUID } from 'crypto'
import { setTimeout as sleep } from 'timers/promises'

type Acquired = boolean

export class LockCode {
  constructor(private readonly code: string) {}

  public getCode(): string {
    return this.code
  }
}

export class LockOwnershipLostError extends Error {}

export class RedisLockLease {
  private readonly abortController = new AbortController()
  private renewalTimer: ReturnType<typeof setTimeout> | null = null
  private renewal: Promise<void> = Promise.resolve()
  private renewalError: unknown
  private isReleased = false

  constructor(
    private readonly provider: RedisLockProvider,
    private readonly key: string,
    private readonly ttl: number,
    private readonly owner: LockCode,
  ) {
    this.scheduleRenewal()
  }

  get signal(): AbortSignal {
    return this.abortController.signal
  }

  async release(): Promise<void> {
    if (this.isReleased) {
      return
    }
    this.isReleased = true
    if (this.renewalTimer) {
      clearTimeout(this.renewalTimer)
    }
    await this.renewal

    let releaseError: unknown
    try {
      await this.withTimeout(
        this.provider.releaseLease(this.key, this.owner),
        this.operationTimeoutMs,
        `Redis lock release timed out for ${this.key}`,
      )
    } catch (error) {
      releaseError = error
    }

    if (this.renewalError) {
      throw this.renewalError
    }
    if (releaseError) {
      throw releaseError
    }
  }

  private get operationTimeoutMs(): number {
    // Two renewal attempts and the delay between them finish by 7/8 TTL.
    return Math.min((this.ttl * 1000) / 8, 1000)
  }

  private scheduleRenewal(): void {
    this.renewalTimer = setTimeout(
      () => {
        this.renewal = this.renewWithTimeout()
          .catch((error) => {
            if (error instanceof LockOwnershipLostError) {
              throw error
            }
            if (this.isReleased) {
              return
            }
            return new Promise<void>((resolve) => setTimeout(resolve, this.operationTimeoutMs)).then(() => {
              if (!this.isReleased) {
                return this.renewWithTimeout()
              }
            })
          })
          .catch((error) => {
            this.renewalError = error
            this.abortController.abort(error)
          })
          .then(() => {
            if (!this.isReleased && !this.renewalError) {
              this.scheduleRenewal()
            }
          })
      },
      (this.ttl * 1000) / 2,
    )
  }

  private async renewWithTimeout(): Promise<void> {
    await this.withTimeout(
      this.provider.renewLease(this.key, this.ttl, this.owner),
      this.operationTimeoutMs,
      `Redis lock renewal timed out for ${this.key}`,
    )
  }

  private async withTimeout(operation: Promise<void>, timeoutMs: number, timeoutMessage: string): Promise<void> {
    let timeout: ReturnType<typeof setTimeout>
    const timeoutPromise = new Promise<never>((_, reject) => {
      timeout = setTimeout(() => reject(new Error(timeoutMessage)), timeoutMs)
    })

    try {
      await Promise.race([operation, timeoutPromise])
    } finally {
      clearTimeout(timeout)
    }
  }
}

export async function withRedisLockLease<T>(
  lease: RedisLockLease,
  operation: (signal: AbortSignal) => Promise<T>,
  onSuppressedReleaseError?: (error: unknown) => void,
): Promise<T> {
  const signal = lease.signal
  let result: T
  try {
    result = await operation(signal)
    signal.throwIfAborted()
  } catch (error) {
    try {
      await lease.release()
    } catch (releaseError) {
      onSuppressedReleaseError?.(releaseError)
    }
    throw error
  }

  await lease.release()
  return result
}

@Injectable()
export class RedisLockProvider {
  private readonly logger = new Logger(RedisLockProvider.name)

  constructor(@InjectRedis() private readonly redis: Redis) {}

  async lock(key: string, ttl: number, code?: LockCode | null): Promise<Acquired> {
    const keyValue = code ? code.getCode() : '1'
    const acquired = await this.redis.set(key, keyValue, 'EX', ttl, 'NX')
    return !!acquired
  }

  async acquireLease(key: string, ttl: number): Promise<RedisLockLease | null> {
    const owner = new LockCode(randomUUID())
    if (!(await this.lock(key, ttl, owner))) {
      return null
    }
    return new RedisLockLease(this, key, ttl, owner)
  }

  async renewLease(key: string, ttl: number, owner: LockCode): Promise<void> {
    const renewed = (await this.redis.eval(
      `if redis.call('get', KEYS[1]) == ARGV[1] then
        return redis.call('expire', KEYS[1], ARGV[2])
      end
      return 0`,
      1,
      key,
      owner.getCode(),
      ttl,
    )) as number
    if (renewed !== 1) {
      throw new LockOwnershipLostError(`Cannot renew Redis lock lease for ${key}: ownership was lost`)
    }
  }

  async releaseLease(key: string, owner: LockCode): Promise<void> {
    const released = (await this.redis.eval(
      `if redis.call('get', KEYS[1]) == ARGV[1] then
        return redis.call('del', KEYS[1])
      end
      return 0`,
      1,
      key,
      owner.getCode(),
    )) as number
    if (released !== 1) {
      this.logger.warn(`Redis lock lease for ${key} was no longer owned when released`)
    }
  }

  async waitForLease(key: string, ttl: number, signal: AbortSignal): Promise<RedisLockLease> {
    while (true) {
      signal.throwIfAborted()
      const acquisition = this.acquireLease(key, ttl)
      let onAbort!: () => void
      const cancelled = new Promise<never>((_, reject) => {
        onAbort = () => reject(signal.reason)
        signal.addEventListener('abort', onAbort, { once: true })
      })

      try {
        const lease = await Promise.race([acquisition, cancelled])
        signal.throwIfAborted()
        if (lease) return lease
      } catch (error) {
        if (signal.aborted) {
          void acquisition
            .then((lease) => lease?.release())
            .catch((releaseError) =>
              this.logger.error(`Error cleaning up cancelled Redis lock wait for ${key}`, releaseError),
            )
        }
        throw error
      } finally {
        signal.removeEventListener('abort', onAbort)
      }

      try {
        await sleep(50, undefined, { signal })
      } catch (error) {
        signal.throwIfAborted()
        throw error
      }
    }
  }

  async getCode(key: string): Promise<LockCode | null> {
    const keyValue = await this.redis.get(key)
    return keyValue ? new LockCode(keyValue) : null
  }

  async unlock(key: string): Promise<void> {
    await this.redis.del(key)
  }

  async isLocked(key: string): Promise<boolean> {
    const exists = await this.redis.exists(key)
    return exists === 1
  }

  async waitForLock(key: string, ttl: number, timeoutMs: number = 30000): Promise<void> {
    const deadline = Date.now() + timeoutMs
    while (Date.now() < deadline) {
      const acquired = await this.lock(key, ttl)
      if (acquired) return
      await new Promise((resolve) => setTimeout(resolve, 50))
    }
    throw new Error(`waitForLock timed out after ${timeoutMs}ms for key: ${key}`)
  }
}
