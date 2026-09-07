#!/usr/bin/env node
import { run } from '../src/run.ts'

const main = async (): Promise<number> => {
  try {
    return await run({ argv: process.argv.slice(2) })
  } catch (error) {
    process.stderr.write(`${(error as Error).message}\n`)
    return 1
  }
}

process.exitCode = await main()
