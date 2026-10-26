import { mount } from 'svelte';

import './assets/web.css';
import App from './App.svelte';
import { startPerfProbe } from './perf';

const target = document.getElementById('app');
if (!target) {
  throw new Error('index.html has no #app to mount into');
}

// The frame probe, before the mount so the boot's own frames are measured
// too. **Every build starts one**: the shell writes its records through its
// own invoke, and a page with no shell sends the same batches over its
// socket once the connection is taken (`perf.ts` holds the run's opening
// line until then).
startPerfProbe();

mount(App, { target });
