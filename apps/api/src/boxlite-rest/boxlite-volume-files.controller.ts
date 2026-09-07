/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { Body, Controller, Delete, Get, HttpCode, Param, Post, Query, UseGuards } from '@nestjs/common'
import { ApiExcludeController } from '@nestjs/swagger'
import { CombinedAuthGuard } from '../auth/combined-auth.guard'
import { OrganizationResourceActionGuard } from '../organization/guards/organization-resource-action.guard'
import { RequiredOrganizationResourcePermissions } from '../organization/decorators/required-organization-resource-permissions.decorator'
import { OrganizationResourcePermission } from '../organization/enums/organization-resource-permission.enum'
import { VolumeAccessGuard } from '../box/guards/volume-access.guard'
import { VolumeFilesService } from '../box/services/volume-files.service'
import {
  BatchDeleteVolumeFilesDto,
  BatchDeleteVolumeFilesResponseDto,
  ListVolumeFilesResponseDto,
  PresignBatchWriteVolumeFilesDto,
  PresignBatchWriteVolumeFilesResponseDto,
  PresignedUrlResponseDto,
  VolumeFileStatDto,
} from '../box/dto/volume-file.dto'

/**
 * File-level operations on a Volume's contents, without attaching it to a
 * box (POL-216). Lives in the Box API dialect, not `box/controllers` -
 * `/files` operations are single-owner Box-contract territory (see the CI
 * "Contract boundary guard"), same reasoning that already put box exec/files
 * behind `boxlite-proxy.controller.ts` rather than the cloud `box.module.ts`.
 */
@Controller(['v1/volumes/:volumeId/files', 'v1/:prefix/volumes/:volumeId/files'])
@ApiExcludeController()
@UseGuards(CombinedAuthGuard, OrganizationResourceActionGuard)
export class BoxliteVolumeFilesController {
  constructor(private readonly volumeFilesService: VolumeFilesService) {}

  @Get()
  @RequiredOrganizationResourcePermissions([OrganizationResourcePermission.READ_VOLUMES])
  @UseGuards(VolumeAccessGuard)
  async listFiles(
    @Param('volumeId') volumeId: string,
    @Query('path') path = '',
    @Query('cursor') cursor?: string,
  ): Promise<ListVolumeFilesResponseDto> {
    return this.volumeFilesService.listFiles(volumeId, path, cursor)
  }

  @Get('stat')
  @RequiredOrganizationResourcePermissions([OrganizationResourcePermission.READ_VOLUMES])
  @UseGuards(VolumeAccessGuard)
  async statFile(@Param('volumeId') volumeId: string, @Query('path') path: string): Promise<VolumeFileStatDto> {
    return this.volumeFilesService.statFile(volumeId, path)
  }

  @Get('presign-read')
  @RequiredOrganizationResourcePermissions([OrganizationResourcePermission.READ_VOLUMES])
  @UseGuards(VolumeAccessGuard)
  async presignRead(
    @Param('volumeId') volumeId: string,
    @Query('path') path: string,
  ): Promise<PresignedUrlResponseDto> {
    return this.volumeFilesService.presignRead(volumeId, path)
  }

  @Post('presign-write')
  @RequiredOrganizationResourcePermissions([OrganizationResourcePermission.WRITE_VOLUMES])
  @UseGuards(VolumeAccessGuard)
  async presignWrite(
    @Param('volumeId') volumeId: string,
    @Query('path') path: string,
  ): Promise<PresignedUrlResponseDto> {
    return this.volumeFilesService.presignWrite(volumeId, path)
  }

  @Delete('content')
  @HttpCode(204)
  @RequiredOrganizationResourcePermissions([OrganizationResourcePermission.WRITE_VOLUMES])
  @UseGuards(VolumeAccessGuard)
  async deleteFile(@Param('volumeId') volumeId: string, @Query('path') path: string): Promise<void> {
    await this.volumeFilesService.deleteFile(volumeId, path)
  }

  @Post('batch-delete')
  @RequiredOrganizationResourcePermissions([OrganizationResourcePermission.WRITE_VOLUMES])
  @UseGuards(VolumeAccessGuard)
  async batchDelete(
    @Param('volumeId') volumeId: string,
    @Body() dto: BatchDeleteVolumeFilesDto,
  ): Promise<BatchDeleteVolumeFilesResponseDto> {
    return this.volumeFilesService.batchDelete(volumeId, dto.paths)
  }

  @Post('presign-batch-write')
  @RequiredOrganizationResourcePermissions([OrganizationResourcePermission.WRITE_VOLUMES])
  @UseGuards(VolumeAccessGuard)
  async presignBatchWrite(
    @Param('volumeId') volumeId: string,
    @Body() dto: PresignBatchWriteVolumeFilesDto,
  ): Promise<PresignBatchWriteVolumeFilesResponseDto> {
    return this.volumeFilesService.presignBatchWrite(volumeId, dto.paths)
  }
}
