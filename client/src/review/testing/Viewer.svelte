<script lang="ts">
  /**
   * The diff viewer, stood in for in tests: the shapes a live page renders,
   * which is the whole of the canvas's contract with it.
   *
   * jsdom cannot mount the real viewer - its text measurement allocates
   * without bound there - so the table below is a capture of the unified
   * shape, the same ones `diff.dom.test.ts` reads the page's decisions off:
   * one number cell holding both sides of a context line, and the anchored
   * line's number span the reconciler finds its row by. Rows at 1 and 4,
   * because a card may be anchored somewhere other than line 1 - a moved
   * thread's resolution draws it away from where the comment was written -
   * a third, line 9, that no hunk in the tests carries: the shape an
   * expanded gap's line draws, which takes no comment door - and a fourth,
   * line 30, far enough below the first hunk that the gap between them is
   * longer than one press fills, which is the shape the directional strips
   * and the below-the-last-hunk row are read off. Each hunk carries its own
   * header row, whose action cell is where the gap controls sit.
   */
</script>

<table>
  <tbody>
    <tr>
      <td class="diff-line-hunk-action"></td>
      <td class="diff-line-hunk-content" colspan="3">@@ -1,1 +1,1 @@</td>
    </tr>
    <tr class="diff-line">
      <td class="diff-line-num"
        ><span data-line-old-num="1">1</span><span data-line-new-num="1">1</span></td
      >
      <td class="diff-line-content">one</td>
    </tr>
    <tr>
      <td class="diff-line-hunk-action"></td>
      <td class="diff-line-hunk-content" colspan="3">@@ -4,1 +4,1 @@</td>
    </tr>
    <tr class="diff-line">
      <td class="diff-line-num"
        ><span data-line-old-num="4">4</span><span data-line-new-num="4">4</span></td
      >
      <td class="diff-line-content">four</td>
    </tr>
    <tr class="diff-line">
      <td class="diff-line-num"
        ><span data-line-old-num="9">9</span><span data-line-new-num="9">9</span></td
      >
      <td class="diff-line-content">nine</td>
    </tr>
    <tr>
      <td class="diff-line-hunk-action"></td>
      <td class="diff-line-hunk-content" colspan="3">@@ -30,1 +30,1 @@</td>
    </tr>
    <tr class="diff-line">
      <td class="diff-line-num"
        ><span data-line-old-num="30">30</span><span data-line-new-num="30">30</span></td
      >
      <td class="diff-line-content">thirty</td>
    </tr>
  </tbody>
</table>
