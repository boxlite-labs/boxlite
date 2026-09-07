/*
 * SES, reached over SMTP.
 *
 * SMTP rather than the SESv2 SDK so the application keeps one vendor-neutral
 * contract — host, port, credential, From — that a self-hosted deploy can point
 * at any provider. Only this file knows the backend is SES.
 *
 * The component publishes the DKIM CNAMEs and the DMARC TXT through the DNS
 * adapter and then blocks the deploy until SES has observed them. That wait is
 * the point: a domain whose zone this token cannot write fails here, once,
 * rather than at the first invitation nobody was there to see bounce.
 *
 * A domain identity is unique per account and region, so exactly one stage may
 * create a given sender domain. Give a stage its own subdomain when its bounces
 * should not touch the shared domain's reputation.
 *
 * Port 465 rather than 587: the session is encrypted from the first byte, so
 * there is no cleartext handshake for a downgrade to strip — and EC2 throttles
 * outbound 25, which the NAT instances these tasks egress through inherit.
 */

import type { Mail, MailProvider, MailRequest } from '../../mail.ts'

/** SES's regional SMTP endpoint. The one place the backend is named. */
const smtpHost = (region: string): string => `email-smtp.${region}.amazonaws.com`

export const awsMailProvider =
  ({ region, dns }: { region: string; dns: ReturnType<typeof sst.cloudflare.dns> }): MailProvider =>
  (request: MailRequest): Mail => {
    if (!request.senderDomain) return { sending: false }

    const identity = new sst.aws.Email('Mail', {
      sender: request.senderDomain,
      dns,
      transform: {
        // The deploy role's SES grant is scoped by name prefix, and SST would
        // satisfy it on its own. Naming it here says so out loud rather than
        // leaving the grant resting on a prefix applied elsewhere — and it is
        // what an operator reading a bounce metric sees.
        configurationSet: (args: any) => {
          args.configurationSetName = `${$app.name}-${$app.stage}-mail`
        },
      },
    })

    return {
      sending: true,
      // SES observed the records before this resource settled, which is what
      // makes the claim true rather than assumed.
      verified: true,
      host: smtpHost(region),
      port: '465',
      // Derived rather than configurable: SES rejects a From address outside
      // the verified identity, so the two must not be able to drift apart.
      senderAddress: `no-reply@${request.senderDomain}`,
      ready: [identity],
    }
  }
