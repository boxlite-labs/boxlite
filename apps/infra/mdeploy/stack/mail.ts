/*
 * Outbound mail for the control plane.
 *
 * The application's contract is SMTP and nothing else: a host, a port, a
 * credential and a From address. That was already true before this module
 * existed — `apps/api` speaks SMTP so a self-hosted deploy can point it at any
 * provider — and it is what makes this module portable at all.
 *
 * The two clouds are not symmetric here and the contract does not pretend they
 * are. AWS has SES, so the provider verifies a domain, publishes DKIM and
 * DMARC, and blocks the deploy until SES has seen the records. Google has no
 * first-party equivalent: a GCP stage sends through whatever relay it is given,
 * and its provider verifies nothing because there is nothing of Google's to
 * verify. `verified` is on the handle so a caller can tell one from the other —
 * a status page that reported "mail verified" on a stage where nobody checked
 * would be worse than one that said nothing.
 *
 * A stage with no sender domain is a stage that sends no mail. That is a
 * supported state rather than a misconfiguration: the API logs it once at boot
 * and every invitation route answers 503 with the reason.
 */

export type MailRequest = {
  /**
   * The verified sender domain, or null for a stage that sends nothing.
   *
   * Null rather than an empty string: an empty domain would compose into a
   * From address of `no-reply@`, which every receiver rejects and no log
   * explains.
   */
  senderDomain: string | null
}

export type Mail =
  | { sending: false }
  | {
      sending: true
      /** True only where something actually checked. See the note above. */
      verified: boolean
      host: $util.Output<string> | string
      port: string
      senderAddress: string
      ready: any[]
    }

export type MailProvider = (request: MailRequest) => Mail

/** The credential's two halves, delivered by reference like every other secret. */
export const MAIL_USER_VARIABLE = 'SMTP_USER'
export const MAIL_PASSWORD_VARIABLE = 'SMTP_PASSWORD'

/**
 * The SMTP settings the API reads.
 *
 * Both halves of the credential are absent here on purpose: they reach the
 * container by reference. What is left is the part that is not secret, and a
 * stage that sends nothing contributes no `SMTP_HOST` at all — which is exactly
 * what the API reads as "email disabled". A host with no credential behind it
 * would instead build a transport that authenticates and is refused on every
 * single send.
 */
export const mailEnvironment = (mail: Mail): Record<string, $util.Output<string> | string> =>
  mail.sending
    ? {
        SMTP_HOST: mail.host,
        SMTP_PORT: mail.port,
        SMTP_SECURE: 'true',
        SMTP_EMAIL_FROM: `BoxLite <${mail.senderAddress}>`,
      }
    : {}
