/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { BadRequestException, ValidationPipe } from '@nestjs/common'
import { CreateBoxDto } from '../dto/create-box.dto'
import { unknownFieldExceptionFactory } from './unknown-field-hint'

// This suite drives the pipe BoxliteBoxController installs, not a hand-built
// error tree — a change that moves the decorator, relaxes `whitelist`, or routes
// creates through a different pipe lands here rather than in production.
const pipe = new ValidationPipe({
  transform: true,
  whitelist: true,
  forbidNonWhitelisted: true,
  exceptionFactory: unknownFieldExceptionFactory(CreateBoxDto),
})
const meta = { type: 'body' as const, metatype: CreateBoxDto }

async function rejectionBody(payload: Record<string, unknown>): Promise<Record<string, any>> {
  try {
    await pipe.transform(payload, meta)
  } catch (e) {
    expect(e).toBeInstanceOf(BadRequestException)
    return (e as BadRequestException).getResponse() as Record<string, any>
  }
  throw new Error(`expected ${JSON.stringify(payload)} to be rejected, but it was accepted`)
}

// ---------------------------------------------------------------------------
// The contract test the original fail-open bug never had: an unknown field must
// be rejected at EVERY level, not just the top one. #1354 turned on
// `forbidNonWhitelisted`; nothing pinned it, so a later re-widening would have
// silently restored "201 and a box that ignored what you asked for".
// ---------------------------------------------------------------------------
describe('CreateBoxDto rejects unknown fields at every level', () => {
  it.each([
    ['top level', { image: 'alpine:latest', totally_bogus_field: 123 }, 'totally_bogus_field'],
    ['network', { image: 'alpine:latest', network: { outbund: { mode: 'disabled' } } }, 'network.outbund'],
    [
      'network.outbound',
      { image: 'alpine:latest', network: { outbound: { mode: 'enabled', allowNet: ['example.com'] } } },
      'network.outbound.allowNet',
    ],
    [
      'network.inbound',
      { image: 'alpine:latest', network: { inbound: { mode: 'enabled', allowNet: ['1.2.3.4'] } } },
      'network.inbound.allowNet',
    ],
  ])('rejects an unknown field at %s', async (_level, payload, _expectedField) => {
    const body = await rejectionBody(payload as Record<string, unknown>)

    // Deliberately asserts only the rejection, not the message. This block
    // pins #1354's protection; the hint/allowed payload is pinned separately
    // below, so removing the hint feature must not make these go red.
    expect(body.statusCode).toBe(400)
  })

  it.each([
    ['network', { image: 'alpine:latest', network: { outbund: { mode: 'disabled' } } }, 'network.outbund'],
    [
      'network.outbound',
      { image: 'alpine:latest', network: { outbound: { mode: 'enabled', allowNet: ['example.com'] } } },
      'network.outbound.allowNet',
    ],
    [
      'network.inbound',
      { image: 'alpine:latest', network: { inbound: { mode: 'enabled', allowNet: ['1.2.3.4'] } } },
      'network.inbound.allowNet',
    ],
  ])('names the offending field by its full path at %s', async (_level, payload, expectedField) => {
    const body = await rejectionBody(payload as Record<string, unknown>)

    // NestJS's default message is just `property allowNet should not exist` —
    // it never says which level the field was written at, which is the whole
    // problem when the same name is valid one level up or down.
    expect(body.message).toContain(`"${expectedField}"`)
  })
})

describe('unknown-field rejections name the field the caller meant', () => {
  it('hints at the correct spelling for a one-letter typo', async () => {
    const body = await rejectionBody({ image: 'alpine:latest', network: { outbund: { mode: 'disabled' } } })

    expect(body.message).toBe('unknown field "network.outbund"')
    expect(body.hint).toBe('did you mean "network.outbound"?')
    expect(body.allowed).toEqual(expect.arrayContaining(['network.outbound', 'network.inbound']))
  })

  it('hints across nesting levels for a camelCase allow_net', async () => {
    const body = await rejectionBody({
      image: 'alpine:latest',
      network: { outbound: { mode: 'enabled', allowNet: ['example.com'] } },
    })

    expect(body.hint).toBe('did you mean "network.outbound.allow_net"?')
    expect(body.allowed).toEqual(expect.arrayContaining(['network.outbound.allow_net', 'network.outbound.mode']))
  })

  // The legacy console API really does take a top-level `public`, so callers
  // write it by habit. Edit distance cannot find `network.inbound.mode` from
  // `public`; the relocation table has to.
  it('points a top-level `public` at network.inbound.mode', async () => {
    const body = await rejectionBody({ image: 'alpine:latest', public: false })

    expect(body.message).toBe('unknown field "public"')
    expect(body.hint).toBe('did you mean "network.inbound.mode"?')
  })

  it('points a top-level allow_net at the nested outbound field', async () => {
    const body = await rejectionBody({ image: 'alpine:latest', allow_net: ['example.com'] })

    expect(body.hint).toBe('did you mean "network.outbound.allow_net"?')
  })

  // Guessing is worse than staying quiet: a wrong `hint` sends the caller to
  // edit a field that was never the problem.
  it('omits hint when nothing is close, but still lists allowed fields', async () => {
    const body = await rejectionBody({ image: 'alpine:latest', totally_bogus_field: 123 })

    expect(body.message).toBe('unknown field "totally_bogus_field"')
    expect(body.hint).toBeUndefined()
    expect(body.allowed).toEqual(expect.arrayContaining(['image', 'name']))
  })

  it('reports every unknown field, not just the first', async () => {
    const body = await rejectionBody({ image: 'alpine:latest', totally_bogus_field: 1, another_bogus: 2 })

    expect(body.message).toContain('"totally_bogus_field"')
    expect(body.message).toContain('"another_bogus"')
    expect(body.message).toContain('unknown fields')
  })
})

// An ordinary constraint failure must keep NestJS's default array-of-strings
// shape. Rewriting it too would break every client and test that reads
// `message` as a list.
describe('non-whitelist validation failures keep the default shape', () => {
  it('leaves a bad enum value alone', async () => {
    const body = await rejectionBody({ image: 'alpine:latest', network: { outbound: { mode: 'sideways' } } })

    expect(Array.isArray(body.message)).toBe(true)
    expect(body.hint).toBeUndefined()
    expect(body.allowed).toBeUndefined()
  })
})

// Valid payloads must be untouched by the factory — it only runs on rejection,
// but a regression that made it throw on success would be invisible above.
describe('valid payloads still pass', () => {
  it('accepts the nested network shape', async () => {
    const dto: CreateBoxDto = await pipe.transform(
      { image: 'alpine:latest', network: { outbound: { mode: 'disabled' }, inbound: { mode: 'disabled' } } },
      meta,
    )

    expect(dto.network?.outbound?.mode).toBe('disabled')
    expect(dto.network?.inbound?.mode).toBe('disabled')
  })
})
