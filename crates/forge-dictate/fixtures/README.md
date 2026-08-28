# forge-dictate regression fixtures

15 clips, 156.7s, 4.8 MB. Real dictation, 3.7s to 20.2s, with natural
disfluencies and silence at the edges. `manifest.json` carries file, source id,
duration, SHA-256, and two baselines per clip.

## The baselines are LOCKED KNOWN-GOOD, not ground truth

`baseline_asr` and `baseline_normalized` are **Superwhisper's model output**, not
a human transcription. Their only job is drift detection: if a dependency bump
changes a transcript, the diff is the signal.

**Never tune a model to match these more closely.** That optimises toward
another model's errors. When output diverges, decide whether ours got worse or
merely different, then either fix the regression or re-lock the baseline
deliberately - with a note saying why.

## Known limit: this corpus is fully blind to a normalizer that stops working

11 of 15 clips are correct no-ops - the ASR output was already clean and the
normalizer rightly changed nothing. Only 4 exercise it.

That makes this a strong ASR gate and an asymmetric normalizer gate. It catches
a normalizer that starts **mangling** clean input. It is **fully** blind to one
that quietly degrades into a **passthrough** - bump s1-mini, have it stop
cleaning entirely, and 11 of 15 clips go green because a passthrough is the
right answer on them. If you add clips, add change-heavy ones.

`15_020s.wav` is an **ASR** fixture, not a normalizer one. The ASR renders GGUF
as "GG, UF" and that survives the whole pipeline intact, which makes it a useful
anchor for ASR drift: if the transcript ever changes there, something moved.

An earlier version of this file called it the repair the normalizer exists to
perform, and the clip whose failure is unambiguous. **That is measured false and
the correction matters, because it inverted what a green means.** s1-mini
normalizes styling, structure and context; it does no vocabulary
reconstruction. Given "P Y torch" and "C U D A" it returns "P-Y torch" and
"C-U-D-A". Superwhisper's own s1-mini left "GG, UF" in place too, which is why
the locked `baseline_normalized` still contains it - the reference output was
telling us this the whole time. Leaving that clip unchanged is correct
behaviour, not a defect.

## Screening rule: the exposure is MEANING, not format

These came from a personal dictation history and are published deliberately.
Anything added must be screened the same way, and **an automated scan is not
screening**. Two independent attempts, both useless:

- A regex over 106 recordings for emails, phones, money, hosts and credentials
  flagged **3**. Manual review found roughly a dozen more: a consulting company,
  tax and income discussion, job applications, named colleagues, internal
  hostnames. None matched a pattern.
- A second regex over the surviving 15, scanning for proper nouns, flagged
  **all 15**. All-of-them is not a finding, it is a broken property.

Pattern matching catches formats. The exposure is a project name, a colleague's
first name, a system nobody outside the company has heard of, a business detail
in passing. Read the transcripts.

Two further rules that cost nothing and close real gaps:

1. **Listen to the audio, don't only read the transcript.** The reference text
   captures what the ASR heard, not background conversation, a radio, or someone
   else in the room.
2. **Get the speaker's explicit approval.** This is someone's voice going into a
   public repo permanently. Ved approved this set clip by clip; that approval is
   the authority for shipping it, not anyone's screening.

## Adding clips

1. Pick for a property the set lacks - normalizer-exercising material first.
2. Read the transcript. Listen to the audio.
3. Get the speaker's approval.
4. Copy in as `NN_DDDs.wav`, add to `manifest.json` with SHA-256 and both
   baselines, and record the baselines as they came out - errors included. An
   ASR error is a feature here.
