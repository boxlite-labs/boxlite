/**
 * ComputerBox Example - Desktop automation
 *
 * Demonstrates:
 * - Creating a desktop environment
 * - Waiting for desktop to be ready
 * - Mouse and keyboard automation
 * - Taking screenshots
 * - Web browser access
 */

const { ComputerBox } = require('@boxlite/boxlite');
const fs = require('fs');

async function main() {
  console.log('=== ComputerBox Example ===\n');

  const desktop = new ComputerBox({
    cpus: 2,
    memoryMib: 2048,
    guiHttpPort: 3000,
    guiHttpsPort: 3001
  });

  try {
    console.log('1. Waiting for desktop to be ready...');
    console.log('   This may take 30-60 seconds on first run...');
    await desktop.waitUntilReady(120);  // 2 minute timeout
    console.log('   ✓ Desktop is ready!\n');

    console.log('2. Getting screen information...');
    const [width, height] = await desktop.getScreenSize();
    console.log(`   Screen size: ${width}x${height}\n`);

    console.log('3. Mouse operations...');
    console.log('   Moving mouse to center of screen...');
    await desktop.mouseMove(width / 2, height / 2);

    const [x, y] = await desktop.cursorPosition();
    console.log(`   Cursor position: ${x}, ${y}\n`);

    console.log('4. Taking screenshot...');
    const screenshot = await desktop.screenshot();
    console.log(`   Screenshot captured: ${screenshot.width}x${screenshot.height} ${screenshot.format}`);
    console.log(`   Data size: ${screenshot.data.length} bytes (base64)\n`);

    // Save screenshot to file
    const screenshotPath = '/tmp/boxlite-screenshot.png';
    fs.writeFileSync(screenshotPath, Buffer.from(screenshot.data, 'base64'));
    console.log(`   Screenshot saved to: ${screenshotPath}\n`);

    console.log('5. Keyboard operations...');
    console.log('   Opening application menu...');
    await desktop.key('alt+F2');  // Open application launcher
    await new Promise(resolve => setTimeout(resolve, 500));

    console.log('   Typing text...');
    await desktop.type('xfce4-terminal');
    await desktop.key('Return');
    await new Promise(resolve => setTimeout(resolve, 1000));

    console.log('   Taking another screenshot...');
    const screenshot2 = await desktop.screenshot();
    const screenshotPath2 = '/tmp/boxlite-screenshot-2.png';
    fs.writeFileSync(screenshotPath2, Buffer.from(screenshot2.data, 'base64'));
    console.log(`   Screenshot saved to: ${screenshotPath2}\n`);

    console.log('6. Web browser access:');
    console.log(`   HTTP:  http://localhost:3000`);
    console.log(`   HTTPS: https://localhost:3001`);
    console.log('   (Note: HTTPS uses self-signed certificate)\n');

    console.log('7. Mouse click operations...');
    await desktop.leftClick();
    console.log('   Left click performed');

    await desktop.doubleClick();
    console.log('   Double click performed');

    await desktop.rightClick();
    console.log('   Right click performed\n');

    console.log('8. Scroll operations...');
    await desktop.scroll(500, 300, 'down', 3);
    console.log('   Scrolled down\n');

    console.log('9. Drag operation...');
    await desktop.leftClickDrag(100, 100, 200, 200);
    console.log('   Drag completed\n');

    console.log('✅ All desktop automation examples completed!');
    console.log('\n💡 Desktop is still running - open the browser to interact:');
    console.log('   http://localhost:3000\n');
    console.log('Press Ctrl+C to exit and cleanup...');

    // Keep running for manual interaction
    await new Promise(resolve => {
      process.on('SIGINT', resolve);
    });
  } finally {
    console.log('\n10. Cleaning up...');
    await desktop.stop();
    console.log('    Desktop stopped and removed.');
  }
}

// Run the example
main().catch(error => {
  console.error('Error:', error);
  process.exit(1);
});
