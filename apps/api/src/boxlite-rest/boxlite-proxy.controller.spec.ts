/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { EventEmitter } from 'node:events'
import { ForbiddenException, RequestTimeoutException } from '@nestjs/common'
import { createProxyMiddleware } from 'http-proxy-middleware'
import { BoxliteProxyController } from './boxlite-proxy.controller'

jest.mock('http-proxy-middleware', () => ({
  createProxyMiddleware: jest.fn(),
  fixRequestBody: jest.fn(),
}))
jest.mock('uuid', () => ({ v4: jest.fn(() => 'mock-uuid'), validate: jest.fn(() => true) }))

const activeAuth = {
  organizationId: 'org-1',
  organization: { id: 'org-1', suspended: false } as any,
}

function makeHarness() {
  const boxService = {
    findOneByIdOrName: jest
      .fn()
      .mockResolvedValue({ id: 'box-uuid', runnerId: 'runner-1', autoResume: true, state: 'started', public: true }),
    updateLastActivityAt: jest.fn().mockResolvedValue(undefined),
    getNetworkTunnelUrl: jest.fn().mockResolvedValue('https://3000-box.proxy.test'),
  }
  const runnerService = {
    findOne: jest.fn().mockResolvedValue({ apiUrl: 'http://runner.local', apiKey: 'runner-key' }),
  }
  const autoResume = { ensureReady: jest.fn().mockResolvedValue(undefined) }
  const controller = new BoxliteProxyController(boxService as never, runnerService as never, autoResume as never)
  return { controller, boxService, autoResume }
}

describe('BoxliteProxyController', () => {
  beforeEach(() => jest.clearAllMocks())
  afterEach(() => jest.useRealTimers())

  it('rewrites public box ids to internal box ids before proxying exec', async () => {
    const proxyHandler = jest.fn()
    jest.mocked(createProxyMiddleware).mockReturnValue(proxyHandler as never)
    const { controller, boxService } = makeHarness()
    const req = { url: '/api/v1/boxes/public-box/exec' }
    const res = {}
    const next = jest.fn()

    await controller.proxyExec(activeAuth as never, 'public-box', req as never, res as never, next)

    const proxyOptions = jest.mocked(createProxyMiddleware).mock.calls[0][0]
    const pathRewrite = proxyOptions.pathRewrite as (path: string, req: unknown) => string
    expect(pathRewrite('/api/v1/boxes/public-box/exec', req)).toBe('/v1/boxes/box-uuid/exec')
    expect(boxService.findOneByIdOrName).toHaveBeenCalledWith('public-box', 'org-1')
    expect(proxyHandler).toHaveBeenCalledWith(req, res, next)
  })

  it('disables the runner timeout for files only', async () => {
    jest.mocked(createProxyMiddleware).mockReturnValue(jest.fn() as never)
    const { controller } = makeHarness()

    await controller.proxyFiles(activeAuth as never, 'public-box', { url: '/files' } as never, {} as never, jest.fn())
    await controller.proxyExec(activeAuth as never, 'public-box', { url: '/exec' } as never, {} as never, jest.fn())

    const fileOptions = jest.mocked(createProxyMiddleware).mock.calls[0][0]
    const execOptions = jest.mocked(createProxyMiddleware).mock.calls[1][0]
    expect(fileOptions.proxyTimeout).toBe(0)
    expect(execOptions.proxyTimeout).toBe(300000)
  })

  // This hook is the only hop carrying the archive-shape hint through the
  // hosted API. If it stops forwarding, every hosted copy_out silently
  // degrades to Unknown and the client re-peeks the archive instead of
  // trusting the guest — a correctness loss with no error to notice.
  it('forwards the archive-shape hint from the runner response', async () => {
    jest.mocked(createProxyMiddleware).mockReturnValue(jest.fn() as never)
    const { controller } = makeHarness()

    await controller.proxyFiles(activeAuth as never, 'public-box', { url: '/files' } as never, {} as never, jest.fn())

    const proxyOptions = jest.mocked(createProxyMiddleware).mock.calls[0][0]
    const proxyRes = proxyOptions.on?.proxyRes as (p: unknown, q: unknown, r: unknown) => void
    const res = { setHeader: jest.fn() }

    proxyRes({ headers: { 'x-boxlite-source-is-dir': 'true' } }, {}, res)

    expect(res.setHeader).toHaveBeenCalledWith('x-boxlite-source-is-dir', 'true')
  })

  // A runner that predates the hint sends no header. Forwarding a fabricated
  // one would tell the client "single file" for a directory stream, so absence
  // must stay absence rather than become a default.
  it('sets no shape header when the runner sends none', async () => {
    jest.mocked(createProxyMiddleware).mockReturnValue(jest.fn() as never)
    const { controller } = makeHarness()

    await controller.proxyFiles(activeAuth as never, 'public-box', { url: '/files' } as never, {} as never, jest.fn())

    const proxyOptions = jest.mocked(createProxyMiddleware).mock.calls[0][0]
    const proxyRes = proxyOptions.on?.proxyRes as (p: unknown, q: unknown, r: unknown) => void
    const res = { setHeader: jest.fn() }

    proxyRes({ headers: {} }, {}, res)

    expect(res.setHeader).not.toHaveBeenCalled()
  })

  it('returns the public endpoint for JSON tunnel requests', async () => {
    const { controller, boxService } = makeHarness()

    const result = await controller.proxyNetworkTunnel(activeAuth as never, 'public-box', 3000)

    expect(boxService.getNetworkTunnelUrl).toHaveBeenCalledWith('public-box', 'org-1', 3000)
    expect(result).toEqual({ uri: 'https://3000-box.proxy.test' })
  })

  it('rejects a tunnel request for a private box with 409', async () => {
    const { controller, boxService } = makeHarness()
    boxService.findOneByIdOrName.mockResolvedValue({
      id: 'box-uuid',
      runnerId: 'runner-1',
      autoResume: true,
      state: 'started',
      public: false,
    })

    await expect(controller.proxyNetworkTunnel(activeAuth as never, 'public-box', 3000)).rejects.toMatchObject({
      status: 409,
    })
    expect(boxService.getNetworkTunnelUrl).not.toHaveBeenCalled()
  })

  it('rejects a stopped, autoResume, private box without waking it', async () => {
    // The visibility gate must run before the resume: waking a box only to
    // reject it here for being private wastes a real resume (and briefly
    // makes a private box's state observable) for a request that was always
    // going to be denied.
    const { controller, boxService, autoResume } = makeHarness()
    boxService.findOneByIdOrName.mockResolvedValue({
      id: 'box-uuid',
      runnerId: 'runner-1',
      autoResume: true,
      state: 'stopped',
      public: false,
    })

    await expect(controller.proxyNetworkTunnel(activeAuth as never, 'public-box', 3000)).rejects.toMatchObject({
      status: 409,
    })
    expect(autoResume.ensureReady).not.toHaveBeenCalled()
    expect(boxService.getNetworkTunnelUrl).not.toHaveBeenCalled()
  })

  it('rejects a tunnel request for a stopped, non-autoResume box with 409', async () => {
    const { controller, boxService, autoResume } = makeHarness()
    boxService.findOneByIdOrName.mockResolvedValue({
      id: 'box-uuid',
      runnerId: 'runner-1',
      autoResume: false,
      state: 'stopped',
    })

    await expect(controller.proxyNetworkTunnel(activeAuth as never, 'public-box', 3000)).rejects.toMatchObject({
      status: 409,
    })
    expect(autoResume.ensureReady).not.toHaveBeenCalled()
    expect(boxService.getNetworkTunnelUrl).not.toHaveBeenCalled()
  })

  it('wakes a stopped autoResume box before minting the tunnel URI (POL-352)', async () => {
    const { controller, boxService, autoResume } = makeHarness()
    boxService.findOneByIdOrName.mockResolvedValue({
      id: 'box-uuid',
      runnerId: 'runner-1',
      autoResume: true,
      state: 'stopped',
      public: true,
    })

    const result = await controller.proxyNetworkTunnel(activeAuth as never, 'public-box', 3000)

    expect(autoResume.ensureReady).toHaveBeenCalledWith('box-uuid', activeAuth.organization)
    expect(boxService.getNetworkTunnelUrl).toHaveBeenCalledWith('public-box', 'org-1', 3000)
    // Order matters: minting the URI before the box is actually STARTED
    // would hand out an address that CONNECTs into a still-stopped box.
    expect(autoResume.ensureReady.mock.invocationCallOrder[0]).toBeLessThan(
      boxService.getNetworkTunnelUrl.mock.invocationCallOrder[0],
    )
    expect(result).toEqual({ uri: 'https://3000-box.proxy.test' })
  })

  it('propagates the resume timeout instead of minting a tunnel URI', async () => {
    const { controller, boxService, autoResume } = makeHarness()
    boxService.findOneByIdOrName.mockResolvedValue({
      id: 'box-uuid',
      runnerId: 'runner-1',
      autoResume: true,
      state: 'stopped',
      public: true,
    })
    const timeout = new RequestTimeoutException('Timed out waiting to resume box box-uuid')
    autoResume.ensureReady.mockRejectedValue(timeout)

    await expect(controller.proxyNetworkTunnel(activeAuth as never, 'public-box', 3000)).rejects.toBe(timeout)
    expect(boxService.getNetworkTunnelUrl).not.toHaveBeenCalled()
  })

  it.each(['creating', 'starting', 'error', 'archived', 'unknown'])(
    'rejects a tunnel request for a non-started box in state %s',
    async (state) => {
      const { controller, boxService } = makeHarness()
      boxService.findOneByIdOrName.mockResolvedValue({
        id: 'box-uuid',
        runnerId: 'runner-1',
        autoResume: false,
        state,
      })

      await expect(controller.proxyNetworkTunnel(activeAuth as never, 'public-box', 3000)).rejects.toMatchObject({
        status: 409,
      })
      expect(boxService.getNetworkTunnelUrl).not.toHaveBeenCalled()
    },
  )

  it('auto-resumes exec and files but treats metrics as observation-only', async () => {
    jest.mocked(createProxyMiddleware).mockReturnValue(jest.fn() as never)
    const { controller, boxService, autoResume } = makeHarness()

    await controller.proxyExec(activeAuth as never, 'public-box', { url: '/exec' } as never, {} as never, jest.fn())
    await controller.proxyFiles(activeAuth as never, 'public-box', { url: '/files' } as never, {} as never, jest.fn())
    expect(autoResume.ensureReady).toHaveBeenCalledTimes(2)
    expect(boxService.updateLastActivityAt).toHaveBeenCalledTimes(2)

    autoResume.ensureReady.mockClear()
    boxService.updateLastActivityAt.mockClear()
    await controller.proxyMetrics(
      activeAuth as never,
      'public-box',
      { url: '/metrics' } as never,
      {} as never,
      jest.fn(),
    )
    expect(autoResume.ensureReady).not.toHaveBeenCalled()
    expect(boxService.updateLastActivityAt).not.toHaveBeenCalled()
  })

  it('does not proxy when the strict AutoResume gate fails', async () => {
    const proxyHandler = jest.fn()
    jest.mocked(createProxyMiddleware).mockReturnValue(proxyHandler as never)
    const { controller, autoResume } = makeHarness()
    autoResume.ensureReady.mockRejectedValue(new Error('start failed'))

    await expect(
      controller.proxyExec(activeAuth as never, 'public-box', { url: '/exec' } as never, {} as never, jest.fn()),
    ).rejects.toThrow('start failed')
    expect(proxyHandler).not.toHaveBeenCalled()
  })

  it('does not auto-resume a box whose autoResume switch is off', async () => {
    jest.mocked(createProxyMiddleware).mockReturnValue(jest.fn() as never)
    const { controller, boxService, autoResume } = makeHarness()
    boxService.findOneByIdOrName.mockResolvedValue({ id: 'box-uuid', runnerId: 'runner-1', autoResume: false })

    await controller.proxyExec(activeAuth as never, 'public-box', { url: '/exec' } as never, {} as never, jest.fn())

    expect(autoResume.ensureReady).not.toHaveBeenCalled()
  })

  it('surfaces suspended-organization failures and never proxies', async () => {
    const proxyHandler = jest.fn()
    jest.mocked(createProxyMiddleware).mockReturnValue(proxyHandler as never)
    const { controller, autoResume } = makeHarness()
    autoResume.ensureReady.mockRejectedValue(new ForbiddenException('Organization is suspended'))

    await expect(
      controller.proxyExec(activeAuth as never, 'public-box', { url: '/exec' } as never, {} as never, jest.fn()),
    ).rejects.toThrow(ForbiddenException)
    expect(proxyHandler).not.toHaveBeenCalled()
  })

  it('refreshes activity for the whole lifetime of a file transfer', async () => {
    jest.useFakeTimers()
    jest.mocked(createProxyMiddleware).mockReturnValue(jest.fn() as never)
    const { controller, boxService } = makeHarness()
    boxService.findOneByIdOrName.mockResolvedValue({
      id: 'box-uuid',
      runnerId: 'runner-1',
      autoResume: true,
      autoStop: 900,
    })
    const res = new EventEmitter()

    await controller.proxyFiles(activeAuth as never, 'public-box', { url: '/files' } as never, res as never, jest.fn())

    expect(boxService.updateLastActivityAt).toHaveBeenCalledTimes(1)

    jest.advanceTimersByTime(450_000)
    expect(boxService.updateLastActivityAt).toHaveBeenCalledTimes(2)

    res.emit('close')
  })

  it('keeps the heartbeat below half the autoStop window for a one-second setting', async () => {
    jest.useFakeTimers()
    jest.mocked(createProxyMiddleware).mockReturnValue(jest.fn() as never)
    const { controller, boxService } = makeHarness()
    boxService.findOneByIdOrName.mockResolvedValue({
      id: 'box-uuid',
      runnerId: 'runner-1',
      autoResume: true,
      autoStop: 1,
    })
    const res = new EventEmitter()

    await controller.proxyFiles(activeAuth as never, 'public-box', { url: '/files' } as never, res as never, jest.fn())
    expect(boxService.updateLastActivityAt).toHaveBeenCalledTimes(1)

    // autoStop=1 → the heartbeat must fire within the window, not at its end.
    jest.advanceTimersByTime(500)
    expect(boxService.updateLastActivityAt).toHaveBeenCalledTimes(2)

    res.emit('close')
  })

  it('stops refreshing activity once the transfer completes', async () => {
    jest.useFakeTimers()
    jest.mocked(createProxyMiddleware).mockReturnValue(jest.fn() as never)
    const { controller, boxService } = makeHarness()
    boxService.findOneByIdOrName.mockResolvedValue({
      id: 'box-uuid',
      runnerId: 'runner-1',
      autoResume: true,
      autoStop: 900,
    })
    const res = new EventEmitter()

    await controller.proxyFiles(activeAuth as never, 'public-box', { url: '/files' } as never, res as never, jest.fn())
    expect(boxService.updateLastActivityAt).toHaveBeenCalledTimes(1)

    res.emit('close')
    jest.advanceTimersByTime(900_000)
    expect(boxService.updateLastActivityAt).toHaveBeenCalledTimes(1)
  })
})
