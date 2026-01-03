/**
 * BrowserBox Example - Browser automation
 *
 * Demonstrates:
 * - Starting a browser with remote debugging
 * - Getting the CDP endpoint
 * - Connecting with Puppeteer (optional)
 */

const { BrowserBox } = require('@boxlite/boxlite');

async function main() {
  console.log('=== BrowserBox Example ===\n');

  console.log('1. Creating BrowserBox with Chromium...');
  const browser = new BrowserBox({
    browser: 'chromium',
    memoryMib: 2048,
    cpus: 2
  });

  try {
    console.log('2. Starting browser...');
    await browser.start();
    console.log('   ✓ Browser started!\n');

    console.log('3. Getting CDP endpoint...');
    const endpoint = browser.endpoint();
    console.log(`   Endpoint: ${endpoint}\n`);

    console.log('4. You can now connect to the browser using:');
    console.log('   - Puppeteer:');
    console.log('     ```javascript');
    console.log(`     const browser = await puppeteer.connect({ browserURL: '${endpoint}' });`);
    console.log('     ```');
    console.log('   - Playwright:');
    console.log('     ```javascript');
    console.log(`     const browser = await chromium.connectOverCDP('${endpoint}');`);
    console.log('     ```\n');

    console.log('5. Example: Connecting with Puppeteer (if installed)...');
    try {
      const puppeteer = require('puppeteer-core');

      console.log('   Connecting to browser...');
      const browserInstance = await puppeteer.connect({ browserURL: endpoint });

      console.log('   Opening new page...');
      const page = await browserInstance.newPage();

      console.log('   Navigating to example.com...');
      await page.goto('https://example.com');

      console.log('   Getting page title...');
      const title = await page.title();
      console.log(`   Page title: ${title}\n`);

      console.log('   Taking screenshot...');
      await page.screenshot({ path: '/tmp/browserbox-screenshot.png' });
      console.log('   Screenshot saved to: /tmp/browserbox-screenshot.png\n');

      console.log('   Closing page...');
      await page.close();

      console.log('   ✅ Puppeteer example completed!');
    } catch (err) {
      console.log('   ⚠️  Puppeteer not found or connection failed');
      console.log(`   Error: ${err.message}`);
      console.log('   Install with: npm install puppeteer-core\n');
    }

    console.log('\nBrowser is still running. Press Ctrl+C to exit...');

    // Keep running for manual interaction
    await new Promise(resolve => {
      process.on('SIGINT', resolve);
    });
  } finally {
    console.log('\n6. Cleaning up...');
    await browser.stop();
    console.log('   Browser stopped and removed.');
  }
}

// Run the example
main().catch(error => {
  console.error('Error:', error);
  process.exit(1);
});
