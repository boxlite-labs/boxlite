/**
 * InteractiveBox Example - Interactive terminal session
 *
 * Demonstrates:
 * - Creating an interactive shell
 * - PTY terminal forwarding
 * - Real-time I/O
 */

const { InteractiveBox } = require('@boxlite/boxlite');

async function main() {
  console.log('=== InteractiveBox Example ===\n');

  console.log('Starting interactive shell...');
  console.log('(Type "exit" to close the session)\n');

  const box = new InteractiveBox({
    image: 'alpine:latest',
    shell: '/bin/sh',
    tty: true,
    memoryMib: 512,
    cpus: 1
  });

  try {
    await box.start();

    // Wait for shell to exit
    await box.wait();

    console.log('\nShell exited.');
  } finally {
    await box.stop();
    console.log('Box cleaned up.');
  }
}

// Run the example
main().catch(error => {
  console.error('Error:', error);
  process.exit(1);
});
