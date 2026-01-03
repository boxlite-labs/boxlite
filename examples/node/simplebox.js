/**
 * SimpleBox Example - Basic command execution
 *
 * Demonstrates:
 * - Creating a simple container
 * - Executing commands
 * - Collecting output
 * - Proper cleanup
 */

const { SimpleBox } = require('@boxlite/boxlite');

async function main() {
  console.log('=== SimpleBox Example ===\n');

  // Create a box with Alpine Linux
  const box = new SimpleBox({ image: 'alpine:latest' });

  try {
    console.log('1. Running basic command...');
    const result1 = await box.exec('echo', 'Hello from BoxLite!');
    console.log(`   Output: ${result1.stdout.trim()}`);
    console.log(`   Exit code: ${result1.exitCode}\n`);

    console.log('2. Listing files...');
    const result2 = await box.exec('ls', '-la', '/');
    console.log(`   Output (first 5 lines):`);
    const lines = result2.stdout.split('\n').slice(0, 5);
    lines.forEach(line => console.log(`   ${line}`));
    console.log();

    console.log('3. Running command with environment variables...');
    const result3 = await box.exec('sh', '-c', 'echo $MY_VAR', { MY_VAR: 'custom-value' });
    console.log(`   Output: ${result3.stdout.trim()}\n`);

    console.log('4. Box metadata...');
    const info = box.info();
    console.log(`   ID: ${box.id}`);
    console.log(`   Name: ${box.name || '(unnamed)'}`);
    console.log(`   Info: ${JSON.stringify(info, null, 2)}\n`);

    console.log('✅ All commands completed successfully!');
  } finally {
    console.log('\n5. Cleaning up...');
    await box.stop();
    console.log('   Box stopped and removed.');
  }
}

// Run the example
main().catch(error => {
  console.error('Error:', error);
  process.exit(1);
});
