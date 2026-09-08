/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { BadRequestException, ValidationError } from '@nestjs/common'
import { getMetadataStorage } from 'class-validator'

/**
 * Turn `forbidNonWhitelisted`'s 400 into one that names the field the caller
 * probably meant.
 *
 * `whitelist` + `forbidNonWhitelisted` (see `BoxliteBoxController`) already stop
 * an unknown field from being silently stripped, which is what closed the
 * fail-open hole where a misspelled `network.outbund` returned 201 and booted a
 * box with no egress policy. What they do not do is say what the right spelling
 * was: class-validator's own message is `property outbund should not exist`.
 *
 * For a one-letter difference, or a field written at the wrong nesting level,
 * that leaves the caller to diff the payload against the spec by eye. This
 * factory adds two fields to the response:
 *
 * - `hint`   — the closest valid field at that level, when one is close enough
 * - `allowed` — every valid field at that level, always
 *
 * Only whitelist violations are rewritten. Ordinary constraint failures keep
 * NestJS's default shape so existing clients and tests are unaffected.
 */

/** class-validator's constraint key for a `forbidNonWhitelisted` violation. */
const WHITELIST_CONSTRAINT = 'whitelistValidation'

/**
 * Fields callers reach for that live somewhere else entirely, where edit
 * distance cannot help.
 *
 * `public` is the load-bearing entry: the legacy console API really does take a
 * top-level `public` boolean, so a developer moving to boxlite-rest writes it by
 * habit. Before strict validation that silently left the box anonymously
 * reachable; now it 400s, and this points at the field that actually controls
 * inbound reachability.
 */
const RELOCATED_FIELDS: Record<string, Record<string, string>> = {
  '': {
    public: 'network.inbound.mode',
    allow_net: 'network.outbound.allow_net',
    networkAllowList: 'network.outbound.allow_net',
    networkBlockAll: 'network.outbound.mode',
  },
}

/**
 * Longest edit distance still treated as a typo rather than a different word.
 * Scales with the name so `mode` cannot match `user` (distance 4 on a 4-char
 * name) while `outbund` still matches `outbound`.
 */
function typoBudget(name: string): number {
  return Math.max(1, Math.floor(name.length * 0.34))
}

/** Levenshtein distance. Iterative single-row; inputs here are field names. */
function editDistance(a: string, b: string): number {
  if (a === b) return 0
  let prev = Array.from({ length: b.length + 1 }, (_, i) => i)
  for (let i = 1; i <= a.length; i++) {
    const row = [i]
    for (let j = 1; j <= b.length; j++) {
      const cost = a[i - 1] === b[j - 1] ? 0 : 1
      row[j] = Math.min(row[j - 1] + 1, prev[j] + 1, prev[j - 1] + cost)
    }
    prev = row
  }
  return prev[b.length]
}

/**
 * Every property class-validator validates on `target`.
 *
 * This is the same metadata `whitelist` consults to decide what to strip, so
 * `allowed` cannot drift from what the pipe actually accepts — a new field with
 * a decorator shows up here automatically, and one without a decorator is
 * genuinely not accepted.
 */
function validatedProperties(target: Function): string[] {
  const metadatas = getMetadataStorage().getTargetValidationMetadatas(target, target.name, false, false)
  return [...new Set(metadatas.map((m) => m.propertyName))].filter(Boolean).sort()
}

const PRIMITIVES: unknown[] = [Object, Array, String, Number, Boolean, Date]

/**
 * The DTO class a nested property holds, from `emitDecoratorMetadata`'s
 * `design:type`. Returns undefined for primitives and for containers, which
 * have no field list of their own to report.
 */
function nestedTarget(target: Function, property: string): Function | undefined {
  const declared: unknown = Reflect.getMetadata?.('design:type', target.prototype, property)
  if (typeof declared !== 'function' || PRIMITIVES.includes(declared)) return undefined
  return declared as Function
}

/** Walk a dotted path from the root DTO, returning the class that owns its last segment. */
function resolveTarget(root: Function, path: string[]): Function | undefined {
  let current: Function | undefined = root
  for (const segment of path) {
    if (!current) return undefined
    current = nestedTarget(current, segment)
  }
  return current
}

interface UnknownField {
  /** Dotted path of the offending field, e.g. `network.outbund`. */
  field: string
  /** Dotted paths of the fields valid at that level. */
  allowed: string[]
  /** Dotted path of the field the caller probably meant, when one is close enough. */
  hint?: string
}

function dotted(prefix: string[], name: string): string {
  return [...prefix, name].join('.')
}

/**
 * Collect whitelist violations across the error tree, resolving each one's
 * level against the DTO so `allowed`/`hint` describe the level the caller wrote
 * at rather than the root.
 */
function collectUnknownFields(errors: ValidationError[], root: Function, prefix: string[] = []): UnknownField[] {
  const found: UnknownField[] = []

  for (const error of errors) {
    if (error.constraints?.[WHITELIST_CONSTRAINT]) {
      const owner = resolveTarget(root, prefix)
      const allowed = owner ? validatedProperties(owner) : []
      const relocated = RELOCATED_FIELDS[prefix.join('.')]?.[error.property]
      const nearest = relocated ? undefined : nearestField(error.property, allowed)

      found.push({
        field: dotted(prefix, error.property),
        allowed: allowed.map((name) => dotted(prefix, name)),
        hint: relocated ?? (nearest ? dotted(prefix, nearest) : undefined),
      })
      continue
    }

    if (error.children?.length) {
      found.push(...collectUnknownFields(error.children, root, [...prefix, error.property]))
    }
  }

  return found
}

/** Closest candidate within the typo budget, or undefined when nothing is close. */
function nearestField(property: string, candidates: string[]): string | undefined {
  const budget = typoBudget(property)
  let best: { name: string; distance: number } | undefined

  for (const candidate of candidates) {
    const distance = editDistance(property.toLowerCase(), candidate.toLowerCase())
    if (distance > budget) continue
    if (!best || distance < best.distance) best = { name: candidate, distance }
  }

  return best?.name
}

/**
 * `exceptionFactory` for a `forbidNonWhitelisted` pipe.
 *
 * Pass the DTO class so nested levels can be resolved; NestJS does not hand the
 * metatype to `exceptionFactory`.
 */
export function unknownFieldExceptionFactory(root: Function) {
  return (errors: ValidationError[]): BadRequestException => {
    const unknown = collectUnknownFields(errors, root)

    // Not an unknown-field rejection — keep NestJS's default shape.
    if (unknown.length === 0) return new BadRequestException(flattenMessages(errors))

    const fields = unknown.map((u) => `"${u.field}"`).join(', ')
    const hints = unknown.filter((u) => u.hint)

    return new BadRequestException({
      statusCode: 400,
      error: 'Bad Request',
      message: `unknown field${unknown.length > 1 ? 's' : ''} ${fields}`,
      ...(hints.length > 0 && {
        hint: hints.map((u) => `did you mean "${u.hint}"?`).join(' '),
      }),
      allowed: [...new Set(unknown.flatMap((u) => u.allowed))],
    })
  }
}

/** NestJS's own default: the flattened constraint messages. */
function flattenMessages(errors: ValidationError[]): string[] {
  return errors.flatMap((error) => [
    ...Object.values(error.constraints ?? {}),
    ...flattenMessages(error.children ?? []),
  ])
}
