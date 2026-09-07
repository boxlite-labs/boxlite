/*
 * Copyright 2026 BoxLite AI
 * SPDX-License-Identifier: AGPL-3.0-only
 */

import { EventEmitter } from 'events'
import type { ExecutionContext, INestApplication } from '@nestjs/common'
import { getRedisConnectionToken } from '@nestjs-modules/ioredis'
import { Test } from '@nestjs/testing'
import type { AddressInfo } from 'net'
import { CombinedAuthGuard } from '../../auth/combined-auth.guard'
import { AuthenticatedRateLimitGuard } from '../../common/guards/authenticated-rate-limit.guard'
import { OrganizationService } from '../../organization/services/organization.service'
import { OrganizationUserService } from '../../organization/services/organization-user.service'
import { BoxAccessGuard } from '../guards/box-access.guard'
import { ProxyGuard } from '../guards/proxy.guard'
import { RegionBoxAccessGuard } from '../guards/region-box-access.guard'
import { RunnerService } from '../services/runner.service'
import { BoxService } from '../services/box.service'
import { BoxController } from './box.controller'

// The real OrganizationResourceActionGuard runs here: it is the class-level
// guard the activity renewal has to get past, so overriding it would test
// nothing. Only the authentication step is stubbed, to mint the context
// ApiKeyStrategy builds for PROXY_API_KEY.
describe('BoxController reached by the preview proxy', () => {
  let app: INestApplication | undefined
  let boxService: {
    updateLastActivityAt: jest.Mock
    findOneByIdOrName: jest.Mock
    getRegionId: jest.Mock
    toBoxDto: jest.Mock
  }

  async function startApp(user: Record<string, unknown>): Promise<void> {
    const subscriber = Object.assign(new EventEmitter(), {
      subscribe: jest.fn().mockResolvedValue(1),
    })
    boxService = {
      updateLastActivityAt: jest.fn().mockResolvedValue(undefined),
      findOneByIdOrName: jest.fn().mockResolvedValue({ id: 'box-1' }),
      // RegionBoxAccessGuard admits a region proxy only for a box in its own
      // region, so the box under test has to live in the region the context names.
      getRegionId: jest.fn().mockResolvedValue('region-1'),
      toBoxDto: jest.fn().mockResolvedValue({ id: 'box-1' }),
    }

    const moduleRef = await Test.createTestingModule({
      controllers: [BoxController],
      providers: [
        BoxAccessGuard,
        ProxyGuard,
        RegionBoxAccessGuard,
        { provide: RunnerService, useValue: {} },
        { provide: BoxService, useValue: boxService },
        { provide: OrganizationService, useValue: { findOne: jest.fn().mockResolvedValue(null) } },
        { provide: OrganizationUserService, useValue: { findOne: jest.fn().mockResolvedValue(null) } },
        {
          provide: getRedisConnectionToken(),
          useValue: {
            duplicate: jest.fn(() => subscriber),
            get: jest.fn().mockResolvedValue(null),
            set: jest.fn().mockResolvedValue('OK'),
          },
        },
      ],
    })
      .overrideGuard(CombinedAuthGuard)
      .useValue({
        canActivate: (context: ExecutionContext) => {
          context.switchToHttp().getRequest().user = user
          return true
        },
      })
      .overrideGuard(AuthenticatedRateLimitGuard)
      .useValue({ canActivate: () => true })
      .compile()

    app = moduleRef.createNestApplication()
    app.setGlobalPrefix('api')
    await app.listen(0)
  }

  afterEach(async () => {
    await app?.close()
    app = undefined
  })

  function request(path: string, method: string): Promise<Response> {
    const address = app!.getHttpServer().address() as AddressInfo
    return fetch(`http://127.0.0.1:${address.port}/api${path}`, { method })
  }

  // The proxy authenticates with PROXY_API_KEY, which ApiKeyStrategy maps to a
  // bare `{ role: 'proxy' }` context — no organizationId, no organization user.
  it('accepts the activity renewal, which declares ProxyGuard', async () => {
    await startApp({ role: 'proxy' })

    const response = await request('/box/box-1/last-activity', 'POST')

    expect(response.status).toBe(201)
    expect(boxService.updateLastActivityAt).toHaveBeenCalledWith('box-1', expect.any(Date))
  })

  it('accepts the renewal from a region proxy, which declares RegionBoxAccessGuard', async () => {
    await startApp({ role: 'region-proxy', regionId: 'region-1' })

    const response = await request('/box/box-1/last-activity', 'POST')

    expect(response.status).toBe(201)
    expect(boxService.updateLastActivityAt).toHaveBeenCalledWith('box-1', expect.any(Date))
  })

  // The exemption is opt-in per route. `getBox` declares only BoxAccessGuard,
  // whose proxy branch admits any proxy context for any box, so the
  // organization guard must stay in front of it.
  it('still rejects a route that declares no proxy guard', async () => {
    await startApp({ role: 'proxy' })

    const response = await request('/box/box-1', 'GET')

    expect(response.status).toBe(403)
    expect(boxService.findOneByIdOrName).not.toHaveBeenCalled()
  })
})
