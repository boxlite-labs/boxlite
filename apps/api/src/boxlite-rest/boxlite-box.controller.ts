/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import {
  Controller,
  Get,
  Post,
  Delete,
  Head,
  Body,
  Param,
  Query,
  HttpCode,
  UseGuards,
  UsePipes,
  ValidationPipe,
  Logger,
  Res,
} from '@nestjs/common'
import { ApiTags, ApiBearerAuth, ApiResponse, ApiExcludeController } from '@nestjs/swagger'
import { Response } from 'express'
import { CombinedAuthGuard } from '../auth/combined-auth.guard'
import { OrganizationResourceActionGuard } from '../organization/guards/organization-resource-action.guard'
import { AuthContext } from '../common/decorators/auth-context.decorator'
import { OrganizationAuthContext } from '../common/interfaces/auth-context.interface'
import { BoxService } from '../box/services/box.service'
import { BoxStateWaiterService } from '../box/services/box-state-waiter.service'
import { Box } from '../box/entities/box.entity'
import { BoxState } from '../box/enums/box-state.enum'
import { BoxDesiredState } from '../box/enums/box-desired-state.enum'
import { BoxResponseDto, ListBoxesResponseDto } from './dto/box-response.dto'
import { CreateBoxDto } from './dto/create-box.dto'
import { unknownFieldExceptionFactory } from './pipes/unknown-field-hint'
import { boxToBoxResponse, createBoxToCreateBox } from './mappers/box-to-box.mapper'
import { Audit, MASKED_AUDIT_VALUE, TypedRequest } from '../audit/decorators/audit.decorator'
import { AuditAction } from '../audit/enums/audit-action.enum'
import { AuditTarget } from '../audit/enums/audit-target.enum'
import { CommerceBoxLimitService } from './commerce-box-limit.service'

// Spec-first surface: the contract is openapi/box.openapi.yaml, not the
// generated product spec (which `:prefix` routes would render invalid).
@ApiExcludeController()
@ApiTags('BoxLite REST')
@Controller(['v1/boxes', 'v1/:prefix/boxes'])
@UseGuards(CombinedAuthGuard, OrganizationResourceActionGuard)
// The spec declares `additionalProperties: false` on CreateBoxRequest, and the
// other two servers enforce it — `boxlite serve` through
// `#[serde(deny_unknown_fields)]`, the reference server through
// `extra="forbid"`. Without this, a body carrying a sandbox knob
// (`security`, `advanced.security`, `privileged`) or a host path
// (`rootfs_path`) is accepted and silently forgotten: the caller gets a 201
// and a box that ignored what they asked for.
//
// Scoped to this controller rather than the global pipe in main.ts, which also
// serves the dashboard, admin, runner-callback and webhook routes — none of
// which are covered by this spec, and all of which would change behaviour in
// the same commit.
//
// `whitelist` alone only strips unknown fields; `forbidNonWhitelisted` is what
// turns a stripped field into a 400. Both are required.
//
// `exceptionFactory` only rewrites whitelist violations, adding `hint` (the
// closest valid field at that level) and `allowed` (every valid field there).
// Stopping the request was the security fix; naming the right field is what
// keeps a one-letter typo from costing the caller a debugging session.
@UsePipes(
  new ValidationPipe({
    transform: true,
    whitelist: true,
    forbidNonWhitelisted: true,
    exceptionFactory: unknownFieldExceptionFactory(CreateBoxDto),
  }),
)
@ApiBearerAuth()
export class BoxliteBoxController {
  private readonly logger = new Logger(BoxliteBoxController.name)

  constructor(
    private readonly boxService: BoxService,
    private readonly boxStateWaiter: BoxStateWaiterService,
    private readonly commerceBoxLimitService: CommerceBoxLimitService,
  ) {}

  @Post()
  @HttpCode(201)
  @ApiResponse({
    status: 201,
    description: 'Box created',
    type: BoxResponseDto,
  })
  @Audit({
    action: AuditAction.CREATE,
    targetType: AuditTarget.BOX,
    targetIdFromResult: (result: BoxResponseDto) => result?.box_id,
    requestMetadata: {
      body: (req: TypedRequest<CreateBoxDto>) => ({
        name: req.body?.name,
        image: req.body?.image,
        user: req.body?.user,
        env: req.body?.env
          ? Object.fromEntries(Object.keys(req.body?.env).map((key) => [key, MASKED_AUDIT_VALUE]))
          : undefined,
        secrets: req.body?.secrets?.map((s) => ({
          name: s.name,
          hosts: s.hosts,
          value: MASKED_AUDIT_VALUE,
        })),
        cpus: req.body?.cpus,
        memory_mib: req.body?.memory_mib,
        disk_size_gb: req.body?.disk_size_gb,
        working_dir: req.body?.working_dir,
        entrypoint: req.body?.entrypoint,
        cmd: req.body?.cmd,
        detach: req.body?.detach,
        auto_stop: req.body?.auto_stop,
        auto_delete: req.body?.auto_delete,
        auto_resume: req.body?.auto_resume,
        network: req.body?.network,
      }),
    },
  })
  async createBox(
    @AuthContext() authContext: OrganizationAuthContext,
    @Body() dto: CreateBoxDto,
  ): Promise<BoxResponseDto> {
    const organization = authContext.organization
    const createBoxDto = createBoxToCreateBox(dto)
    const maxCreatedBoxes = await this.commerceBoxLimitService.resolveMaxCreatedBoxes(organization.id)

    let box = await this.boxService.create(createBoxDto, organization, { maxCreatedBoxes })
    if (box.state !== BoxState.STARTED) {
      box = await this.boxStateWaiter.waitForStarted(box.id, organization.id, 30)
    }
    return boxToBoxResponse(box)
  }

  @Get()
  @ApiResponse({
    status: 200,
    description: 'List boxes',
    type: ListBoxesResponseDto,
  })
  async listBoxes(
    @AuthContext() authContext: OrganizationAuthContext,
    @Query('pageSize') pageSize?: string,
  ): Promise<ListBoxesResponseDto> {
    const boxes = await this.boxService.findAllDeprecated(authContext.organizationId)
    const dtos = await this.boxService.toBoxDtos(boxes)
    return {
      boxes: dtos.map(boxToBoxResponse),
    }
  }

  @Get(':boxId')
  @ApiResponse({
    status: 200,
    description: 'Box details',
    type: BoxResponseDto,
  })
  async getBox(
    @AuthContext() authContext: OrganizationAuthContext,
    @Param('boxId') boxId: string,
  ): Promise<BoxResponseDto> {
    const box = await this.boxService.findOneByIdOrName(boxId, authContext.organizationId)
    const dto = await this.boxService.toBoxDto(box)
    return boxToBoxResponse(dto)
  }

  @Head(':boxId')
  async headBox(
    @AuthContext() authContext: OrganizationAuthContext,
    @Param('boxId') boxId: string,
    @Res() res: Response,
  ) {
    try {
      await this.boxService.findOneByIdOrName(boxId, authContext.organizationId)
      res.status(204).end()
    } catch {
      res.status(404).end()
    }
  }

  @Delete(':boxId')
  @HttpCode(204)
  @Audit({
    action: AuditAction.DELETE,
    targetType: AuditTarget.BOX,
    targetIdFromRequest: (req) => req.params.boxId,
  })
  async removeBox(@AuthContext() authContext: OrganizationAuthContext, @Param('boxId') boxId: string) {
    await this.boxService.destroy(boxId, authContext.organizationId)
  }

  @Post(':boxId/start')
  @ApiResponse({
    status: 201,
    description: 'Box start requested',
    type: BoxResponseDto,
  })
  @Audit({
    action: AuditAction.START,
    targetType: AuditTarget.BOX,
    targetIdFromRequest: (req) => req.params.boxId,
    targetIdFromResult: (result: BoxResponseDto) => result?.box_id,
  })
  async startBox(
    @AuthContext() authContext: OrganizationAuthContext,
    @Param('boxId') boxId: string,
  ): Promise<BoxResponseDto> {
    let box = await this.boxService.findOneByIdOrName(boxId, authContext.organizationId)

    if (this.isStartAlreadyInProgress(box)) {
      const dto = await this.boxStateWaiter.waitForStarted(box.id, authContext.organizationId, 30)
      return boxToBoxResponse(dto)
    }

    box = await this.boxService.start(boxId, authContext.organization)
    let dto = await this.boxService.toBoxDto(box)
    if (dto.state !== BoxState.STARTED) {
      dto = await this.boxStateWaiter.waitForStarted(box.id, authContext.organizationId, 30)
    }
    return boxToBoxResponse(dto)
  }

  @Post(':boxId/stop')
  @ApiResponse({
    status: 201,
    description: 'Box stop requested',
    type: BoxResponseDto,
  })
  @Audit({
    action: AuditAction.STOP,
    targetType: AuditTarget.BOX,
    targetIdFromRequest: (req) => req.params.boxId,
    targetIdFromResult: (result: BoxResponseDto) => result?.box_id,
  })
  async stopBox(
    @AuthContext() authContext: OrganizationAuthContext,
    @Param('boxId') boxId: string,
  ): Promise<BoxResponseDto> {
    const box = await this.boxService.stop(boxId, authContext.organizationId)
    const dto = await this.boxService.toBoxDto(box)
    return boxToBoxResponse(dto)
  }

  private isStartAlreadyInProgress(box: Box): boolean {
    return (
      box.desiredState === BoxDesiredState.STARTED &&
      [BoxState.UNKNOWN, BoxState.CREATING, BoxState.STARTING, BoxState.RESTORING].includes(box.state)
    )
  }
}
