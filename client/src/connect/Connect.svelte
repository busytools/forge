<script lang="ts">
  import { untrack } from 'svelte';

  import Brand from '../components/Brand.svelte';
  import Field from '../composer/Field.svelte';
  import type { ClientSettings } from '../wire/types';
  import { DEFAULT_ADDRESS, submitAttempt, type Attempt } from './attempt';

  let {
    settings,
    initialAddress = DEFAULT_ADDRESS,
    launchFailure = null,
    onconnect,
  }: {
    settings: ClientSettings;
    /** What the field opens on: the remembered address, or the default. */
    initialAddress?: string;
    /** Why a launch did not open the home, drawn on the door as it lands. */
    launchFailure?: Extract<Attempt, { ok: false }> | null;
    onconnect: (connected: Extract<Attempt, { ok: true }>) => void;
  } = $props();

  // Seeded once, then the field's own: what the shell hands down is where the
  // door OPENS, not something it keeps following.
  let address = $state(untrack(() => initialAddress));
  let busy = $state(false);
  let submitted = $state<Extract<Attempt, { ok: false }> | null>(null);

  /**
   * The last submit's reason, which stands over the launch's while it does.
   *
   * The launch's is a PROP rather than a seed, and that is the point: a launch
   * can finish while this screen is already mounted - at `/connect`, or on the
   * door a reload lands on - and a value copied once at mount would never
   * arrive. Seeded instead of followed, this screen is indistinguishable from
   * one where nothing was ever tried.
   */
  const failure = $derived(submitted ?? launchFailure);

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    busy = true;
    submitted = null;
    // No `finally`: `submitAttempt` cannot reject, so every path out of a
    // connection arrives here as one of two shapes and the button is
    // re-enabled on both. A throw would leave it disabled reading
    // "Connecting", which is the same screen as a connection still running.
    const next = await submitAttempt(address);
    busy = next.busy;
    submitted = next.failure;
    if (next.connected) onconnect(next.connected);
  }
</script>

<!-- A landmark, so every part of the page sits inside one. -->
<main class="wrap">
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
        <Field
          editor="connect"
          element="input"
          id="address"
          name="address"
          autocapitalize="off"
          aria={{
            invalid: failure !== null,
            describedBy: failure ? 'why' : undefined,
          }}
          bind:value={address}
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
        {:else if failure.kind === 'version'}
          <!--
            Not a connection problem, and nothing here can talk it round: a
            forge ahead of this client draws against a protocol this one has
            no way to read.
          -->
          <p class="h">
            That forge speaks a protocol this client does not. The two halves have to match, so one
            of them needs updating.
          </p>
        {/if}
      </div>
    {/if}
  </div>
</main>

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

  /* The box is the shared `Field`, whose element is not this component's own
     markup, so a plain `input` selector no longer reaches it. Under `.field`
     rather than bare, so the rule still stops at this screen. */
  .field :global(input) {
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

  .field :global(input:focus-visible) {
    outline: 2px solid var(--accent);
    outline-offset: -1px;
  }

  .field :global(input[aria-invalid='true']) {
    border-color: var(--bad);
  }

  /* iOS Safari zooms the whole viewport for a field under 16px and leaves the
     page scrolled off-centre, which is the "opens in the wrong position"
     symptom. A desktop responsive mode does not reproduce it. */
  @media (max-width: 560px) {
    .field :global(input) {
      font-size: 16px;
    }
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
