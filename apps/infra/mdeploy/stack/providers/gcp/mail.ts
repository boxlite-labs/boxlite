/*
 * Outbound mail on GCP, which Google does not provide.
 *
 * There is no SES here. Google Cloud has no first-party sending service at all
 * — the documented answer is a third-party relay — so this provider verifies
 * nothing, publishes no DKIM records, and builds no resource. What it does is
 * report the SMTP settings the stage was given and say plainly that nobody
 * checked them.
 *
 * `verified: false` is the whole point of this file. The AWS side blocks the
 * deploy until SES has observed the DKIM and DMARC records, so `verified: true`
 * there is a fact. Returning the same value here would be a claim nothing
 * supports, and the first person to read a status page would believe it.
 *
 * The relay's own credential arrives the way every other secret does: through
 * the store's `api` group, under `SMTP_USER` and `SMTP_PASSWORD`. That is not a
 * GCP concession — it is how the AWS side works too, because an SES SMTP
 * credential is an IAM user's access key and the deploy role cannot mint one
 * either.
 */

import type { Mail, MailProvider, MailRequest } from '../../mail.ts'

/**
 * The relay this stage sends through.
 *
 * A setting rather than a constant, because unlike SES there is no answer this
 * cloud supplies. A stage that names a sender domain and no relay is a stage
 * whose invitations would go nowhere, so it is refused here rather than at the
 * first send.
 */
export const gcpMailProvider =
  ({ relayHost }: { relayHost: string | null }): MailProvider =>
  (request: MailRequest): Mail => {
    if (!request.senderDomain) return { sending: false }
    if (!relayHost) {
      throw new Error(
        `MAIL_DOMAIN names ${request.senderDomain} but this stage declares no MAIL_RELAY_HOST. ` +
          'Google Cloud has no sending service, so a GCP stage sends through a relay it is told about ' +
          'or sends nothing at all — clear MAIL_DOMAIN to turn mail off deliberately.',
      )
    }
    return {
      sending: true,
      // Nothing here checked anything. See the note above.
      verified: false,
      host: relayHost,
      port: '465',
      senderAddress: `no-reply@${request.senderDomain}`,
      ready: [],
    }
  }
