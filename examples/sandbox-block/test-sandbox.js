#!/usr/bin/env node
// Test script to verify sandbox blocks sensitive file access
// Run with: sandbox-exec -p "$(nary-sandbox-profile)" node test-sandbox.js

const fs = require('fs');
const path = require('path');
const os = require('os');

const home = os.homedir();

const sensitiveFiles = [
  '.ssh/id_rsa',
  '.ssh/id_ed25519',
  '.ssh/config',
  '.aws/credentials',
  '.aws/config',
  '.gnupg/secring.gpg',
  '.config/gh/hosts.yml',
  '.netrc',
  '.kube/config',
  '.docker/config.json',
];

let blocked = 0;
let accessible = 0;

console.log('Testing sandbox file access restrictions...\n');

for (const file of sensitiveFiles) {
  const fullPath = path.join(home, file);
  try {
    fs.accessSync(fullPath, fs.constants.R_OK);
    // If we get here, file exists and is readable - BAD if sandboxed
    console.log(`  ACCESSIBLE: ${file}`);
    accessible++;
  } catch (err) {
    if (err.code === 'ENOENT') {
      console.log(`  NOT FOUND:  ${file}`);
    } else if (err.code === 'EACCES' || err.code === 'EPERM') {
      console.log(`  BLOCKED:    ${file}`);
      blocked++;
    } else {
      console.log(`  ERROR:      ${file} (${err.code})`);
    }
  }
}

console.log(`\nResults: ${blocked} blocked, ${accessible} accessible`);

if (accessible > 0) {
  console.log('\n⚠️  WARNING: Some sensitive files are accessible!');
  console.log('   If running in sandbox, this indicates a sandbox escape.');
  process.exit(1);
} else if (blocked > 0) {
  console.log('\n✓ Sandbox is working - sensitive files are blocked');
  process.exit(0);
} else {
  console.log('\n? No sensitive files found to test');
  process.exit(0);
}
