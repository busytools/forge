import { mount } from 'svelte';

import './assets/web.css';
import App from './App.svelte';
import { startPerfProbe } from './perf';

const target = document.getElementById('app');
if (!target) {
  throw new Error('index.html has no #app to mount into');
}

// The frame probe, before the mount so the boot's own frames are measured
// too. A build with no shell starts nothing - `perf.ts` decides that, so
// the web half pays no cost for a probe it could not write.
startPerfProbe();

mount(App, { target });
