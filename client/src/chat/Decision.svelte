<script lang="ts">
  import type { Decision } from './decisions';
  import { renderInlineProse } from './prose';
  import { joinedLine } from './text';

  /**
   * One System One decision, drawn as the block the mock settled: the
   * question it answered, the answer first, its distribution with each
   * option's own description under it, and what it cost along the bottom.
   *
   * The verdict line draws only what the result carries. What the session
   * then did with the answer - applied it, escalated it - is not on the wire,
   * so nothing here states it.
   */
  let { decision }: { decision: Decision } = $props();

  /** More than half reads as yes; the number stays exactly as returned. */
  const YES = 0.5;

  /**
   * Where a noul's number stops reading as sure, and starts reading as a coin
   * flip. Presentation only: the number beside it is the whole truth.
   */
  const SURE = 0.8;
  const UNSURE = 0.6;

  /** A number as the block writes it: two decimals at most, zeros trimmed. */
  function num(value: number): string {
    return String(Math.round(value * 100) / 100);
  }

  /** The width a bar fills, as a percentage at the same grain; none, none. */
  function width(value: number | null): string {
    return value === null ? '0%' : `${Math.round(value * 10_000) / 100}%`;
  }

  /** How sure a noul is: the winning side's share, whichever side won. */
  const noulConfidence = $derived(
    decision.answer.kind === 'noul'
      ? Math.max(decision.answer.noul, 1 - decision.answer.noul)
      : null,
  );

  /** A choice's confidence, or null for every other answer. */
  const choiceConfidence = $derived(
    decision.answer.kind === 'choice' ? decision.answer.confidence : null,
  );

  /** The number a noul answered with, or null for every other answer. */
  const noulValue = $derived(decision.answer.kind === 'noul' ? decision.answer.noul : null);

  /** The option a choice picked, or null for every other answer. */
  const choicePick = $derived(decision.answer.kind === 'choice' ? decision.answer.choice : null);

  /** The number a score answered with, or null for every other answer. */
  const scoreValue = $derived(decision.answer.kind === 'score' ? decision.answer.score : null);

  /** Whether the noul's number draws as a sure one. */
  const sure = $derived(noulConfidence !== null && noulConfidence >= SURE);

  /** And whether it draws as a coin flip. */
  const unsure = $derived(noulConfidence !== null && noulConfidence <= UNSURE);

  /**
   * The distribution's rows: a name, its value, and whether the answer marks
   * it. A score level the result left without a probability keeps its row
   * with an empty track - the verdict above still names it, and skipping it
   * made the two disagree.
   */
  const rows = $derived.by((): { name: string; value: number | null; win: boolean }[] => {
    const answer = decision.answer;
    if (answer.kind === 'noul') {
      const yes = answer.noul >= YES;
      // The no side is derived: the result returns one probability, and the
      // two sides of a yes/no are complements.
      return [
        { name: 'yes', value: answer.noul, win: yes },
        { name: 'no', value: 1 - answer.noul, win: !yes },
      ];
    }
    if (answer.kind === 'choice') {
      if (answer.probabilities === null) return [];
      return answer.probabilities.map((held) => ({ ...held, win: held.name === answer.choice }));
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
    }));
  });

  /** Where a score sits among its levels, or null when the levels are absent. */
  const scoreNote = $derived.by((): string | null => {
    const answer = decision.answer;
    if (answer.kind !== 'score' || answer.levels === null) return null;
    const floor = Math.floor(answer.score);
    const ceil = Math.ceil(answer.score);
    const name = (at: number): string => answer.levels?.[at]?.name ?? String(at);
    return floor === ceil
      ? `of ${answer.levels.length - 1} - at ${name(floor)}`
      : `of ${answer.levels.length - 1} - between ${name(floor)} and ${name(ceil)}`;
  });
</script>

<div class="dec">
  {#if decision.question !== null}
    <!-- The whole question over the answer: the call's own row holds one line
         of it, and the block used to draw none - so expanding showed the
         answer with nothing saying what it answered (Ved, 2026-10-04). -->
    <div class="q">
      <!-- Rendered from escaped input: the same renderer the row's own line
           and a thought's row use. -->
      <!-- eslint-disable-next-line svelte/no-at-html-tags -->
      {@html renderInlineProse(joinedLine(decision.question))}
    </div>
  {/if}
  <div class="verdict">
    {#if noulValue !== null}
      <span class="num" class:sure class:unsure>{num(noulValue)}</span>
      <span class="read">{noulValue >= YES ? 'yes' : 'no'}</span>
    {:else if choicePick !== null}
      <span class="read"
        >pick: <b>{choicePick}</b
        >{#if choiceConfidence !== null}{` - confidence ${num(choiceConfidence)}`}{/if}</span
      >
    {:else if scoreValue !== null}
      <span class="num">{num(scoreValue)}</span>
      {#if scoreNote !== null}<span class="read">{scoreNote}</span>{/if}
    {/if}
  </div>

  {#if rows.length > 0}
    <div class="dist">
      {#each rows as row, at (at)}
        <div class="opt" class:win={row.win}>
          <div class="oline">
            <span class="name">{row.name}</span>
            <span class="track"
              ><span class="fill" class:win={row.win} style={`width:${width(row.value)}`}
              ></span></span
            >
            <span class="p">{row.value === null ? '' : num(row.value)}</span>
          </div>
          {#if decision.criteria[row.name] !== undefined}
            <div class="crit">{decision.criteria[row.name]}</div>
          {/if}
        </div>
      {/each}
    </div>
  {/if}

  {#if decision.usage !== null}
    <div class="foot">
      {#if decision.model !== ''}<span>{decision.model}</span><span class="sep">/</span>{/if}
      <span>{decision.usage.input_tokens} in - {decision.usage.output_tokens} out</span>
      {#if decision.usage.cost !== null}<span class="sep">/</span><span
          >${String(decision.usage.cost)}</span
        >{/if}
    </div>
  {/if}
</div>
