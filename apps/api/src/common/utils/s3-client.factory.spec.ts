import { createS3Client } from './s3-client.factory'

function fakeConfig(values: Record<string, unknown>) {
  return {
    get: (key: string) => values[key],
    getOrThrow: (key: string) => {
      if (!(key in values)) {
        throw new Error(`missing config: ${key}`)
      }
      return values[key]
    },
  } as never
}

describe('createS3Client endpoint scheme', () => {
  it('defaults a bare production-looking endpoint to https', () => {
    const client = createS3Client(
      fakeConfig({ 's3.endpoint': 's3.example-region.amazonaws.com', 's3.region': 'us-east-1' }),
    )

    expect(client.config.endpoint).toBeDefined()
    // @aws-sdk/client-s3 stores the endpoint as a resolver; assert via the
    // resolved value instead of reaching into SDK internals. Assert the
    // protocol too, not just the hostname - an http endpoint with the same
    // hostname would otherwise satisfy this test just as well.
    return expect(client.config.endpoint()).resolves.toMatchObject({
      hostname: 's3.example-region.amazonaws.com',
      protocol: 'https:',
    })
  })

  it('keeps a bare minio endpoint on http', async () => {
    const client = createS3Client(
      fakeConfig({
        's3.endpoint': 'minio.internal:9000',
        's3.region': 'us-east-1',
        's3.accessKey': 'a',
        's3.secretKey': 'b',
      }),
    )

    await expect(client.config.endpoint()).resolves.toMatchObject({ protocol: 'http:' })
  })

  it('keeps a bare localhost endpoint on http', async () => {
    const client = createS3Client(
      fakeConfig({
        's3.endpoint': 'localhost:9000',
        's3.region': 'us-east-1',
        's3.accessKey': 'a',
        's3.secretKey': 'b',
      }),
    )

    await expect(client.config.endpoint()).resolves.toMatchObject({ protocol: 'http:' })
  })

  it('does not override an endpoint that already names a scheme', async () => {
    const client = createS3Client(
      fakeConfig({ 's3.endpoint': 'http://s3.example.com', 's3.region': 'us-east-1' }),
    )

    await expect(client.config.endpoint()).resolves.toMatchObject({ protocol: 'http:' })
  })
})
