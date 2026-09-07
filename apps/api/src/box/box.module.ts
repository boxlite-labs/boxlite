/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { Module } from '@nestjs/common'
import { DataSource } from 'typeorm'
import { BoxController } from './controllers/box.controller'
import { BoxService } from './services/box.service'
import { TypeOrmModule } from '@nestjs/typeorm'
import { Box } from './entities/box.entity'
import { UserModule } from '../user/user.module'
import { RunnerService } from './services/runner.service'
import { Runner } from './entities/runner.entity'
import { RunnerController } from './controllers/runner.controller'
import { BoxManager } from './managers/box.manager'
import { BoxMigrationManager } from './managers/box-migration.manager'
import { RedisLockProvider } from './common/redis-lock.provider'
import { OrganizationModule } from '../organization/organization.module'
import { BoxWarmPoolService } from './services/box-warm-pool.service'
import { WarmPool } from './entities/warm-pool.entity'
import { PreviewController } from './controllers/preview.controller'
import { VolumeController } from './controllers/volume.controller'
import { VolumeService } from './services/volume.service'
import { VolumeFilesService } from './services/volume-files.service'
import { VolumeManager } from './managers/volume.manager'
import { Volume } from './entities/volume.entity'
import { VolumeSubscriber } from './subscribers/volume.subscriber'
import { RunnerSubscriber } from './subscribers/runner.subscriber'
import { RunnerAdapterFactory } from './runner-adapter/runnerAdapter'
import { BoxStartAction } from './managers/box-actions/box-start.action'
import { BoxStopAction } from './managers/box-actions/box-stop.action'
import { BoxDestroyAction } from './managers/box-actions/box-destroy.action'
import { BoxRepository } from './repositories/box.repository'
import { RegionModule } from '../region/region.module'
import { Region } from '../region/entities/region.entity'
import { JobController } from './controllers/job.controller'
import { JobService } from './services/job.service'
import { JobStateHandlerService } from './services/job-state-handler.service'
import { Job } from './entities/job.entity'
import { BoxLookupCacheInvalidationService } from './services/box-lookup-cache-invalidation.service'
import { BoxAccessGuard } from './guards/box-access.guard'
import { RunnerAccessGuard } from './guards/runner-access.guard'
import { RegionRunnerAccessGuard } from './guards/region-runner-access.guard'
import { RegionBoxAccessGuard } from './guards/region-box-access.guard'
import { ProxyGuard } from './guards/proxy.guard'
import { EventEmitter2 } from '@nestjs/event-emitter'
import { BoxLastActivity } from './entities/box-last-activity.entity'
import { BoxMigration } from './entities/box-migration.entity'
import { BoxActivityService } from './services/box-activity.service'
import { BoxStateWaiterService } from './services/box-state-waiter.service'
import { BoxMigrationService } from './services/box-migration.service'
import { BoxMigrationJobReceiver } from './services/box-migration-job-receiver.service'

@Module({
  imports: [
    UserModule,
    OrganizationModule,
    RegionModule,
    TypeOrmModule.forFeature([Box, Runner, WarmPool, Volume, Region, Job, BoxLastActivity, BoxMigration]),
  ],
  controllers: [BoxController, RunnerController, PreviewController, VolumeController, JobController],
  providers: [
    BoxService,
    BoxManager,
    BoxWarmPoolService,
    RunnerService,
    BoxLookupCacheInvalidationService,
    RedisLockProvider,
    VolumeService,
    VolumeFilesService,
    VolumeManager,
    VolumeSubscriber,
    RunnerSubscriber,
    RunnerAdapterFactory,
    BoxStartAction,
    BoxStopAction,
    BoxDestroyAction,
    JobService,
    JobStateHandlerService,
    BoxActivityService,
    BoxStateWaiterService,
    BoxMigrationService,
    BoxMigrationManager,
    BoxMigrationJobReceiver,
    BoxAccessGuard,
    RunnerAccessGuard,
    RegionRunnerAccessGuard,
    RegionBoxAccessGuard,
    ProxyGuard,
    {
      provide: BoxRepository,
      inject: [DataSource, EventEmitter2, BoxLookupCacheInvalidationService],
      useFactory: (
        dataSource: DataSource,
        eventEmitter: EventEmitter2,
        boxLookupCacheInvalidationService: BoxLookupCacheInvalidationService,
      ) => new BoxRepository(dataSource, eventEmitter, boxLookupCacheInvalidationService),
    },
  ],
  exports: [
    BoxService,
    RunnerService,
    RedisLockProvider,
    VolumeService,
    VolumeFilesService,
    VolumeManager,
    BoxRepository,
    RunnerAdapterFactory,
    BoxActivityService,
    BoxStateWaiterService,
  ],
})
export class BoxModule {}
