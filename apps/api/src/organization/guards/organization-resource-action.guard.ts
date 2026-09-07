/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { CanActivate, Injectable, ExecutionContext, Logger, Type } from '@nestjs/common'
import { GUARDS_METADATA } from '@nestjs/common/constants'
import { Reflector } from '@nestjs/core'
import { OrganizationAccessGuard } from './organization-access.guard'
import { RequiredOrganizationResourcePermissions } from '../decorators/required-organization-resource-permissions.decorator'
import { OrganizationMemberRole } from '../enums/organization-member-role.enum'
import { OrganizationService } from '../services/organization.service'
import { OrganizationUserService } from '../services/organization-user.service'
import { BaseAuthContext, OrganizationAuthContext } from '../../common/interfaces/auth-context.interface'
import { SystemRole } from '../../user/enums/system-role.enum'
import { isRunnerContext } from '../../common/interfaces/runner-context.interface'
import { isProxyContext } from '../../common/interfaces/proxy-context.interface'
import { isRegionProxyContext } from '../../common/interfaces/region-proxy.interface'
import { OR_GUARD_INNER_GUARDS } from '../../auth/or.guard'

// Machine identities carry no organization — ApiKeyStrategy mints a bare
// { role } context for the runner, the proxy and the region proxy — so
// organization-scoped access can never be satisfied by one. A handler opts a
// machine role in by declaring one of that role's own guards; guards are matched
// by name because importing them here would close a module cycle.
//
// `ProxyGuard` alone speaks for the proxy. BoxAccessGuard admits any proxy
// context for any box, so listing it under the proxy would widen every route
// that uses it, not just the routes meant for the proxy.
const MACHINE_ROLE_RESOURCE_GUARD_NAMES: ReadonlyArray<
  readonly [(user: BaseAuthContext) => boolean, ReadonlySet<string>]
> = [
  [isRunnerContext, new Set(['RunnerAuthGuard', 'BoxAccessGuard'])],
  [isProxyContext, new Set(['ProxyGuard'])],
  [isRegionProxyContext, new Set(['RegionBoxAccessGuard'])],
]

@Injectable()
export class OrganizationResourceActionGuard extends OrganizationAccessGuard {
  protected readonly logger = new Logger(OrganizationResourceActionGuard.name)

  constructor(
    organizationService: OrganizationService,
    organizationUserService: OrganizationUserService,
    private readonly reflector: Reflector,
  ) {
    super(organizationService, organizationUserService)
  }
  async canActivate(context: ExecutionContext): Promise<boolean> {
    const request = context.switchToHttp().getRequest()
    if (this.handlerAllowsMachineResourceAccess(context, request.user)) {
      return true
    }

    const canActivate = await super.canActivate(context)

    // TODO: initialize authContext safely
    const authContext: OrganizationAuthContext = request.user
    if (!authContext) {
      return false
    }

    if (authContext.role === SystemRole.ADMIN) {
      return true
    }

    if (!canActivate) {
      return false
    }

    if (!authContext.organizationUser) {
      return false
    }

    if (authContext.organizationUser.role === OrganizationMemberRole.OWNER && !authContext.apiKey) {
      return true
    }

    const requiredPermissions =
      this.reflector.get(RequiredOrganizationResourcePermissions, context.getHandler()) ||
      this.reflector.get(RequiredOrganizationResourcePermissions, context.getClass())

    if (!requiredPermissions) {
      return true
    }

    const assignedPermissions = authContext.apiKey
      ? new Set(authContext.apiKey.permissions)
      : new Set(authContext.organizationUser.assignedRoles.flatMap((role) => role.permissions))

    return requiredPermissions.every((permission) => assignedPermissions.has(permission))
  }

  private handlerAllowsMachineResourceAccess(context: ExecutionContext, user: BaseAuthContext | undefined): boolean {
    if (!user) {
      return false
    }

    const allowedGuardNames = MACHINE_ROLE_RESOURCE_GUARD_NAMES.find(([isRole]) => isRole(user))?.[1]
    if (!allowedGuardNames) {
      return false
    }

    const guards =
      this.reflector.getAllAndMerge<Array<unknown>>(GUARDS_METADATA, [context.getHandler(), context.getClass()]) ?? []
    return guards.some((guard) => this.guardAllowsResourceAccess(guard, allowedGuardNames))
  }

  private guardAllowsResourceAccess(guard: unknown, allowedGuardNames: ReadonlySet<string>): boolean {
    if (allowedGuardNames.has(this.guardName(guard))) {
      return true
    }

    const innerGuards = (guard as { [OR_GUARD_INNER_GUARDS]?: Type<CanActivate>[] })?.[OR_GUARD_INNER_GUARDS]
    return innerGuards?.some((innerGuard) => this.guardAllowsResourceAccess(innerGuard, allowedGuardNames)) ?? false
  }

  private guardName(guard: unknown): string {
    return typeof guard === 'function' ? guard.name : ''
  }
}
