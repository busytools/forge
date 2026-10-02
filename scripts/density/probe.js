/**
 * The in-page half of the density instrument.
 *
 * Runs inside the probe page (see `measure.mjs`, which assembles it) and
 * returns one JSON object. Everything it reports is read off a real layout:
 * `getBoundingClientRect` for geometry, `getComputedStyle` for the drawn
 * style, and a canvas for the advance and the ink of the face.
 *
 * **The face is asserted, not assumed.** The sheet declares no `--ui` /
 * `--mono` - the server injects them - so a probe page that supplies its own
 * stack measures the fallback. The callers assert on `face.family` and
 * `face.advance` (see `EXPECT` in `measure.mjs`), which is what makes a
 * fallback reading fail loudly instead of being published as Fira Code.
 */

const FACE = 'Fira Code';
const PROSE_CHARS = 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789';
const INK = 'HHHHHHHHHH';

/** The computed value of `prop` on the first match of `sel`, or null. */
function computed(sel, prop) {
  const element = document.querySelector(sel);
  return element === null ? null : getComputedStyle(element).getPropertyValue(prop);
}

/**
 * Advance and ink of `text` at `size` px and `weight`, on a canvas.
 *
 * `width` is the advance the layout uses; `ink` is
 * `actualBoundingBoxRight - actualBoundingBoxLeft`, the extent the glyphs
 * actually cover. A real bold cut moves both; a synthesized one thickens the
 * strokes and so moves `ink` while leaving the advance alone, which is the
 * reading that says whether the page has a bold face or only a smeared
 * regular.
 */
function glyph(text, size, weight) {
  const context = document.createElement('canvas').getContext('2d');
  context.font = `${weight} ${size}px "${FACE}"`;
  const metrics = context.measureText(text);
  return {
    weight,
    width: Number(metrics.width.toFixed(4)),
    ink: Number((metrics.actualBoundingBoxRight + metrics.actualBoundingBoxLeft).toFixed(4)),
  };
}

/** WCAG relative luminance of an `rgb()`/`rgba()` string. */
function luminance(value) {
  const parts = value.match(/[\d.]+/g);
  if (parts === null) return null;
  const channels = parts.slice(0, 3).map((part) => {
    const channel = Number(part) / 255;
    return channel <= 0.03928 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * channels[0] + 0.7152 * channels[1] + 0.0722 * channels[2];
}

/**
 * The contrast a mark is actually drawn at, from its computed colours.
 *
 * The theme tokens are injected by the server, so they are only real on a page
 * that carries them - which this one does, from `client/src/theme.ts`.
 */
function contrast(sel) {
  const element = document.querySelector(sel);
  if (element === null) return null;
  const style = getComputedStyle(element);
  // A transparent background means the mark sits on whatever is behind it, so
  // the reading is only true once that ground is found rather than assumed.
  let ground = element;
  let background = style.backgroundColor;
  while (ground !== null && /rgba?\(0, 0, 0, 0\)|transparent/.test(background)) {
    ground = ground.parentElement;
    if (ground !== null) background = getComputedStyle(ground).backgroundColor;
  }
  const one = luminance(style.color);
  const two = luminance(background);
  if (one === null || two === null) return null;
  const ratio = (Math.max(one, two) + 0.05) / (Math.min(one, two) + 0.05);
  return {
    color: style.color,
    ground: background,
    ratio: Number(ratio.toFixed(2)),
    size: style.fontSize,
    weight: style.fontWeight,
  };
}

/** Every face the page actually has, which is what a weight must be cut at. */
function declaredFaces() {
  const faces = [];
  document.fonts.forEach((face) => {
    faces.push({ family: face.family, weight: face.weight, status: face.status });
  });
  return faces;
}

/**
 * The block list: every child of `.prose`, its drawn height, and the gap
 * between it and the next.
 *
 * The gap is `next.top - this.bottom`, taken from the rects, so a margin
 * collapse between the two blocks reads as the smaller of the two margins
 * exactly as the layout resolved it rather than as the sum of two
 * declarations.
 */
function blocks() {
  const prose = document.querySelector('.prose');
  if (prose === null) return [];
  const children = [...prose.children];
  return children.map((element, index) => {
    const rect = element.getBoundingClientRect();
    const next = children[index + 1];
    const style = getComputedStyle(element);
    const lineHeight = style.lineHeight;
    const parsed = Number.parseFloat(lineHeight);
    // The heaviest weight the block draws, taken over the block and every
    // descendant: a `<p>` that carries a `<strong>` computes 400 on itself, so
    // the element's own font-weight cannot answer whether the block is bold.
    const weights = [element, ...element.querySelectorAll('*')].map(
      (node) => Number.parseInt(getComputedStyle(node).fontWeight, 10),
    );
    return {
      tag: element.tagName.toLowerCase(),
      cls: typeof element.className === 'string' ? element.className : '',
      family: style.fontFamily,
      text: (element.textContent ?? '').trim().slice(0, 40),
      height: Number(rect.height.toFixed(2)),
      fontSize: Number.parseFloat(style.fontSize),
      lineHeight: lineHeight === 'normal' ? null : Number(parsed.toFixed(2)),
      lineUnits: lineHeight === 'normal' || Number.isNaN(parsed)
        ? null
        : Number((rect.height / parsed).toFixed(3)),
      heaviest: Math.max(...weights),
      gapAfter: next === undefined
        ? null
        : Number((next.getBoundingClientRect().top - rect.bottom).toFixed(2)),
    };
  });
}

/**
 * Set the chat column so prose fits exactly `target` characters, then report
 * where it landed.
 *
 * The two views are compared at the same line length: the terminal is a
 * character grid, so its width is a column count, and the client's column
 * has to be the pixel width that fits that many characters or the height
 * ratio is really a wrapping ratio. The loop is a measurement rather than an
 * assumption - it reads back the fitted count and stops when it is exact.
 */
function fitColumn(target) {
  const root = document.documentElement;
  const advance = glyph(PROSE_CHARS, Number.parseFloat(computed('.prose p', 'font-size')), 400)
    .width / PROSE_CHARS.length;
  // The column is counted in the browser's own `ch`, not in the canvas
  // advance: canvas reports the advance rounded to four decimals, and a width
  // that lands exactly on a whole character can then read one character short.
  // `100ch` is measured off a real element, so the unit is the layout's own
  // and the two divide exactly.
  const unit = (() => {
    const probe = document.createElement('div');
    probe.style.cssText = 'position:absolute;visibility:hidden;width:100ch';
    document.querySelector('.prose').append(probe);
    const width = probe.getBoundingClientRect().width / 100;
    probe.remove();
    return width;
  })();
  const whole = (px) => Math.floor((px + unit * 1e-6) / unit);
  // `null` leaves the column wherever the window put it, which is the reading
  // the issue's "the measure is unbounded" hypothesis needs: this is the
  // number the window alone would give.
  if (target === null) {
    const content = document.querySelector('.prose').getBoundingClientRect().width;
    return {
      width: root.getBoundingClientRect().width,
      content,
      fits: whole(content),
      advance,
      settled: true,
    };
  }
  let width = Math.round(target * advance) + 40;
  let content = 0;
  for (let attempt = 0; attempt < 12; attempt += 1) {
    root.style.width = `${width}px`;
    // The prose block's own width, not the scroll viewport's: `.conv` reserves
    // a scrollbar gutter, and the column the text actually wraps in is what
    // the character count has to be taken from.
    content = document.querySelector('.prose').getBoundingClientRect().width;
    const fits = whole(content);
    if (fits === target) return { width, content, fits, advance, settled: true };
    width += Math.round((target - fits) * advance);
  }
  return { width, content, fits: whole(content), advance, settled: false };
}

/** The whole reading, as one object. */
async function measure(targetChars) {
  await document.fonts.ready;
  const fit = fitColumn(targetChars);
  const strong = document.querySelector('.prose strong');
  const proseSize = Number.parseFloat(computed('.prose p', 'font-size'));
  return {
    face: {
      asked: computed('.prose p', 'font-family'),
      family: computed('.prose p', 'font-family'),
      proseSize,
      advance: Number(
        (glyph(PROSE_CHARS, proseSize, 400).width / PROSE_CHARS.length).toFixed(4),
      ),
      canon: computed('.code pre', 'font-family'),
      codeSize: Number.parseFloat(computed('.code pre', 'font-size')),
      declared: declaredFaces(),
    },
    weight: {
      strong: strong === null ? null : getComputedStyle(strong).fontWeight,
      prose: computed('.prose p', 'font-weight'),
      th: computed('.prose th', 'font-weight'),
      canvas: [
        glyph(INK, proseSize, 400),
        glyph(INK, proseSize, 500),
        glyph(INK, proseSize, 700),
      ],
    },
    lineHeight: {
      body: computed('body', 'line-height'),
      prose: computed('.prose p', 'line-height'),
      code: computed('.code pre', 'line-height'),
      table: computed('.prose td', 'line-height'),
    },
    contrast: {
      inlineCode: contrast('.prose code'),
      tableHead: contrast('.prose th'),
      tableCell: contrast('.prose td'),
      blockquote: contrast('.prose blockquote'),
    },
    column: {
      targetChars,
      pageWidth: fit.width,
      contentWidth: Number(fit.content.toFixed(2)),
      fits: fit.fits,
      settled: fit.settled !== false,
    },
    blocks: blocks(),
  };
}

window.__density = measure;
