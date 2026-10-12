<script lang="ts">
  import Chevron from '../components/Chevron.svelte';
  import { num, type Decision, type DecisionState } from './decisions';
  import { renderInlineProse } from './prose';

  /**
   * One System One decision, drawn as the picked record: the question it
   * answered, the answer as a facts line, the options as a plain list in the
   * order they were given, the material it judged behind one fold, and what
   * it cost along the bottom (Ved, 2026-10-12).
   *
   * Low-key, like the other tools' cards: no box, no big number, no bars. The
   * answer is the one loud element - the row's chip and the winner's own row -
   * and everything else is quiet data.
   *
   * The facts line draws only what the result carries. What the session then
   * did with the answer - applied it, escalated it - is not on the wire, so
   * nothing here states it.
   *
   * A structured value draws its own words where it has them, and the value
   * in full behind a disclosure under its row (Ved, 2026-10-06): the block
   * never drops what a call carried.
   */
  let { decision }: { decision: Decision } = $props();

  /** The question's own raw value, drawn onto its open like every row's body. */
  let questionRaw = $state(false);

  /** The state, drawn onto its own open: a long state costs nothing closed. */
  let stateOpen = $state(false);

  /** The options' raw values, by row, drawn onto their own open. */
  let optionRaw = $state<Record<number, boolean>>({});

  /** More than half reads as yes; the number stays exactly as returned. */
  const YES = 0.5;

  /** The words a disclosure leads with, by what the value holds. */
  function disclosureWords(raw: unknown, what: string): string {
    if (Array.isArray(raw)) {
      return `${what} - ${raw.length} ${raw.length === 1 ? 'item' : 'items'}`;
    }
    if (typeof raw === 'object' && raw !== null) {
      const fields = Object.keys(raw).length;
      return `${what} - ${fields} ${fields === 1 ? 'field' : 'fields'}`;
    }
    return what;
  }

  /** A value in full, as its disclosure draws it. */
  function pretty(raw: unknown): string {
    return JSON.stringify(raw, null, 2) ?? String(raw);
  }

  /** The number a noul answered with, or null for every other answer. */
  const noulValue = $derived(decision.answer.kind === 'noul' ? decision.answer.noul : null);

  /** The option a choice picked, or null for every other answer. */
  const choicePick = $derived(decision.answer.kind === 'choice' ? decision.answer.choice : null);

  /** A choice's confidence, or null for every other answer. */
  const choiceConfidence = $derived(
    decision.answer.kind === 'choice' ? decision.answer.confidence : null,
  );

  /** The number a score answered with, or null for every other answer. */
  const scoreValue = $derived(decision.answer.kind === 'score' ? decision.answer.score : null);

  /** And how settled its distribution was, which the old card parsed and never drew. */
  const scoreConfidence = $derived(
    decision.answer.kind === 'score' ? decision.answer.confidence : null,
  );

  /** How many levels the score was given, or null where the call named none. */
  const scoreLevels = $derived(
    decision.answer.kind === 'score' && decision.answer.levels !== null
      ? decision.answer.levels.length - 1
      : null,
  );

  /** One drawn row: its name and value, its mark, its words, its own value. */
  interface Drawn {
    name: string;
    value: number | null;
    win: boolean;
    /** The description line under the row, or null where the call gave none. */
    crit: string | null;
    /** The value in full where it was not a string, for the disclosure. */
    raw: unknown;
  }

  /**
   * The options' rows: a name, its value, and whether the answer marks it.
   *
   * The order is the read's own - the caller's criteria order, which is what
   * the model saw - and a score level the result left without a probability
   * keeps its row, because the facts line above still counts it.
   */
  const rows = $derived.by((): Drawn[] => {
    const answer = decision.answer;
    /** What the criteria map holds for one row's name, where it holds any. */
    const said = (name: string): { crit: string | null; raw: unknown } => ({
      crit: decision.criteria[name]?.text ?? null,
      raw: decision.criteria[name]?.raw ?? null,
    });
    if (answer.kind === 'noul') {
      const yes = answer.noul >= YES;
      // The no side is derived: the result returns one probability, and the
      // two sides of a yes/no are complements.
      return [
        { name: 'yes', value: answer.noul, win: yes, ...said('yes') },
        { name: 'no', value: 1 - answer.noul, win: !yes, ...said('no') },
      ];
    }
    if (answer.kind === 'choice') {
      if (answer.probabilities === null) return [];
      return answer.probabilities.map((held) => ({
        ...held,
        win: held.name === answer.choice,
        ...said(held.name),
      }));
    }
    if (answer.levels === null) return [];
    // The two levels the score sits between carry the mark; a score landing
    // exactly on a level has one.
    const floor = Math.floor(answer.score);
    const ceil = Math.ceil(answer.score);
    return answer.levels.map((held, at) => ({
      name: held.name,
      value: held.value,
      win: at === floor || at === ceil,
      crit: null,
      raw: held.raw,
    }));
  });

  /** What the state's fold line says: its own name and the size behind it. */
  function foldWords(state: DecisionState): string {
    return state.size === null ? 'state' : `state - ${state.size}`;
  }

  /** The state as prose, for the state that arrived as one, or null. */
  const stateText = $derived(
    decision.state !== null && decision.state.kind === 'prose' ? decision.state.text : null,
  );

  /** The state's lines, for the state that arrived as fields or rows, or null. */
  const stateLines = $derived(
    decision.state !== null && decision.state.kind === 'lines' ? decision.state.lines : null,
  );

  /** The meta run along the bottom: what answered, what it cost. */
  const foot = $derived.by((): [string, string][] => {
    const parts: [string, string][] = [];
    if (decision.model !== '') parts.push(['model', decision.model]);
    if (decision.usage !== null) {
      parts.push([
        'tokens',
        `${decision.usage.input_tokens} in / ${decision.usage.output_tokens} out`,
      ]);
      if (decision.usage.cost !== null) parts.push(['cost', `$${String(decision.usage.cost)}`]);
    }
    return parts;
  });
</script>

<div class="dec">
  {#if decision.question !== null}
    <!-- The whole question over the answer: the row's own line cuts it, and
         the block used to join it to one line with its block marks stripped
         (Ved, 2026-10-12) - so a reader expanding it saw the answer with only
         a fragment of what it answered. -->
    <div class="fam-tag">question</div>
    <div class="fam-quote">
      <!-- Rendered from escaped input: the same renderer the row's own line
           and a thought's row use. -->
      <!-- eslint-disable-next-line svelte/no-at-html-tags -->
      {@html renderInlineProse(decision.question.text)}
    </div>
    {#if decision.question.raw !== null}
      <details class="raw" bind:open={questionRaw}>
        <summary
          ><Chevron />{disclosureWords(decision.question.raw, 'structured instructions')}</summary
        >
        {#if questionRaw}<pre>{pretty(decision.question.raw)}</pre>{/if}
      </details>
    {/if}
  {/if}

  <!-- The answer, as the other cards state their facts: one run of label and
       value, no big number and no box. -->
  <div class="fam-meta answer">
    {#if noulValue !== null}
      <span class="lb">noul</span>{num(noulValue)}<span class="sep">&#183;</span><span class="lb"
        >reads</span
      >{noulValue >= YES ? 'yes' : 'no'}
    {:else if choicePick !== null}
      <span class="lb">pick</span>{choicePick}{#if choiceConfidence !== null}<span class="sep"
          >&#183;</span
        ><span class="lb">confidence</span>{num(choiceConfidence)}{/if}
    {:else if scoreValue !== null}
      <span class="lb">score</span>{num(
        scoreValue,
      )}{#if scoreLevels !== null}{` of ${scoreLevels}`}{/if}{#if scoreConfidence !== null}<span
          class="sep">&#183;</span
        ><span class="lb">confidence</span>{num(scoreConfidence)}{/if}
    {/if}
  </div>

  {#if rows.length > 0}
    <div class="fam-list">
      {#each rows as row, at (at)}
        <div class="fam-opt" class:win={row.win}>
          <div class="fam-item">
            <span class="fam-sub">{row.name}</span>
            <span class="fam-when">{row.value === null ? '' : num(row.value)}</span>
          </div>
          {#if row.crit !== null}
            <div class="fam-desc">{row.crit}</div>
          {/if}
          {#if row.raw !== null}
            <details class="raw" bind:open={optionRaw[at]}>
              <summary><Chevron />{disclosureWords(row.raw, 'structured value')}</summary>
              {#if optionRaw[at]}<pre>{pretty(row.raw)}</pre>{/if}
            </details>
          {/if}
        </div>
      {/each}
    </div>
  {/if}

  {#if decision.state !== null}
    <!-- The material the decision was made on, behind one line naming its
         size: the one thing that answers what all was used in the call, and
         the longest part of most of them. -->
    <details class="fold" bind:open={stateOpen}>
      <summary><Chevron />{foldWords(decision.state)}</summary>
      {#if stateOpen}
        <div class="in">
          {#if stateText !== null}
            <div class="fam-quote st-str">{stateText}</div>
          {:else if stateLines !== null}
            {#each stateLines as line, at (at)}
              <div class="st-row" style={`padding-left:${line.depth * 14}px`}>
                <span class="st-k">{line.key ?? ''}</span>
                <span class="st-v" class:dim={line.dim}>{line.text}</span>
              </div>
            {/each}
          {/if}
        </div>
      {/if}
    </details>
  {/if}

  {#if foot.length > 0}
    <div class="fam-meta">
      {#each foot as [label, value], at (at)}{#if at > 0}<span class="sep">&#183;</span>{/if}<span
          class="lb">{label}</span
        >{value}{/each}
    </div>
  {/if}
</div>
