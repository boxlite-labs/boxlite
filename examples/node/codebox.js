/**
 * CodeBox Example - Python code execution
 *
 * Demonstrates:
 * - Running Python code safely
 * - Installing packages
 * - Handling execution results
 * - Error handling
 */

const { CodeBox } = require('@boxlite/boxlite');

async function main() {
  console.log('=== CodeBox Example ===\n');

  const codebox = new CodeBox();

  try {
    console.log('1. Running simple Python code...');
    const result1 = await codebox.run('print("Hello from Python!")');
    console.log(`   Output: ${result1.trim()}\n`);

    console.log('2. Running code with calculations...');
    const code2 = `
import math
result = math.sqrt(144)
print(f"Square root of 144 is {result}")
`.trim();
    const result2 = await codebox.run(code2);
    console.log(`   Output: ${result2.trim()}\n`);

    console.log('3. Installing package and using it...');
    console.log('   Installing requests...');
    await codebox.installPackage('requests');
    console.log('   Package installed!');

    const code3 = `
import requests
response = requests.get('https://api.github.com/zen')
print(f"GitHub Zen: {response.text}")
`.trim();
    const result3 = await codebox.run(code3);
    console.log(`   Output: ${result3.trim()}\n`);

    console.log('4. Installing multiple packages...');
    await codebox.installPackages('numpy', 'pillow');
    console.log('   Packages installed!\n');

    const code4 = `
import numpy as np
arr = np.array([1, 2, 3, 4, 5])
print(f"Array: {arr}")
print(f"Sum: {arr.sum()}")
print(f"Mean: {arr.mean()}")
`.trim();
    const result4 = await codebox.run(code4);
    console.log(`   Output:`);
    result4.trim().split('\n').forEach(line => console.log(`   ${line}`));
    console.log();

    console.log('5. Handling errors in code...');
    try {
      await codebox.run('print(undefined_variable)');
    } catch (err) {
      console.log(`   Caught expected error: ${err.message.split('\n')[0]}`);
    }
    console.log();

    console.log('✅ All examples completed successfully!');
  } finally {
    console.log('\n6. Cleaning up...');
    await codebox.stop();
    console.log('   CodeBox stopped and removed.');
  }
}

// Run the example
main().catch(error => {
  console.error('Error:', error);
  process.exit(1);
});
