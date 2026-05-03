# Fine-tuning Guide

## Start with embeddings first

Do not fine-tune first.

The recommended path is:

```text
reference wav -> XTTS generation -> cache
```

Then:

```text
reference wav -> embedding -> stable voice identity
```

Only then consider fine-tuning.

## When fine-tuning makes sense

Fine-tune only when:

1. The voice identity drifts too much.
2. The reference wav is not enough.
3. You want a stable voice pack.
4. You have clean data and transcripts.

## Data requirements

Minimum:

```text
20 to 30 minutes clean speech
```

Better:

```text
45 to 60 minutes clean speech
```

Best:

```text
1 to 2 hours
```

## Dataset structure

```text
datasets/my_voice/
├── audio/
│   ├── 001.wav
│   ├── 002.wav
└── metadata.csv
```

`metadata.csv`:

```text
001.wav|The build failed again.
002.wav|Command completed successfully.
```

## Training concept

You are not training speech from scratch.

You are adapting a pretrained model:

```text
pretrained TTS model + clean voice dataset -> specialized voice model
```

## Consistency after fine-tuning

Fine-tuning improves identity and style.

It still does not guarantee bit-identical audio.

For identical repeated playback, cache outputs.
