<script lang="ts">
  import Brand from '../components/Brand.svelte';
  import type { ClientSettings } from '../wire/types';
  import { connectTo, DEFAULT_ADDRESS, type Attempt } from './attempt';

  let {
    settings,
    onconnect,
  }: {
    settings: ClientSettings;
    onconnect: (connected: { url: string; settings: ClientSettings }) => void;
  } = $props();

  let address = $state(DEFAULT_ADDRESS);
  let busy = $state(false);
  let failure = $state<Extract<Attempt, { ok: false }> | null>(null);

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    busy = true;
    failure = null;
    const attempt = await connectTo(address);
    busy = false;
    if (attempt.ok) {
      onconnect({ url: attempt.url, settings: attempt.settings });
      return;
    }
    failure = attempt;
  }
</script>

<div class="wrap">
  <div class="door">
    <header class="brand">
      <Brand name={settings.mark} />
      <span class="word">forge</span>
    </header>

    <p class="lede">
      This app connects to a forge that is already running. Point it at one, and it draws the
      projects, the seats and their conversations.
    </p>

    <form onsubmit={submit} novalidate>
      <label for="address">The address forge is serving on</label>
      <div class="field">
        <input
          id="address"
          name="address"
          type="text"
          autocomplete="off"
          autocapitalize="off"
          spellcheck="false"
          bind:value={address}
          aria-invalid={failure !== null}
          aria-describedby={failure ? 'why' : undefined}
        />
        <button type="submit" disabled={busy}>{busy ? 'Connecting' : 'Connect'}</button>
      </div>
    </form>

    {#if failure}
      <div class="no" id="why" role="alert">
        <p class="r">{failure.why}</p>
        {#if failure.kind === 'unreachable'}
          <!--
            A connection refused from outside is the same answer whether the
            server is down or the socket was never switched on, and the screen
            cannot tell them apart. Naming the key is what stops the reader
            going to look at their network.
          -->
          <p class="h">
            Nothing answered there. If forge is running, check that <code>[web] enabled</code> is
            not set to <code>false</code> in <code>forge.toml</code> - a forge whose owner turned the
            socket off refuses in silence, with nothing wrong at either end.
          </p>
        {/if}
      </div>
    {/if}
  </div>
</div>

<style>
  .door {
    max-width: 30rem;
    margin: 12vh auto 0;
    display: flex;
    flex-direction: column;
    gap: 18px;
  }

  /* `.brand`, `.brand .mark` and `.brand .word` come from web.css: the header
     brand is one component and this page draws it, not a copy of it. */

  .lede {
    color: var(--muted);
    font-size: var(--fs-base);
  }

  form {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  label {
    font-size: var(--fs-label);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--dim);
  }

  .field {
    display: flex;
    gap: 8px;
  }

  input {
    flex: 1 1 auto;
    min-width: 0;
    background: var(--s1);
    border: 1px solid var(--line);
    border-radius: var(--r);
    padding: 10px 12px;
    color: var(--text);
    font-family: var(--mono);
    font-size: var(--fs-data);
  }

  input:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -1px;
  }

  input[aria-invalid='true'] {
    border-color: var(--bad);
  }

  button {
    flex: 0 0 auto;
    background: var(--s2);
    border: 1px solid var(--line);
    border-radius: var(--r);
    padding: 10px 16px;
    color: var(--text);
    font-family: var(--ui);
    font-size: var(--fs-base);
    font-weight: 600;
    cursor: pointer;
  }

  button:hover:not(:disabled) {
    border-color: var(--accent);
    color: var(--accent);
  }

  button:disabled {
    color: var(--dim);
    cursor: default;
  }

  .no {
    border: 1px solid var(--line);
    border-left: 3px solid var(--bad);
    border-radius: var(--r);
    background: var(--s1);
    padding: 12px 14px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .no .r {
    color: var(--bad);
    font-size: var(--fs-base);
  }

  .no .h {
    color: var(--muted);
    font-size: var(--fs-data);
  }

  code {
    font-family: var(--mono);
    color: var(--text);
  }
</style>
