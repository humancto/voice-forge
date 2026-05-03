# Build Prompt for AI Coding Agent

Build this repo into a working MVP.

## Context

This repo is VoiceForge Terminal: a local voice runtime for terminal events.

## Main task

Implement a Rust CLI called `voiceforge` and a Python TTS server.

## Features to implement

1. `voiceforge say --text "..."`
2. `voiceforge say --text "..." --voice angry_duck`
3. `voiceforge run -- <command>`
4. Cache audio by SHA256 hash.
5. Call local TTS server at `http://localhost:5000/tts`.
6. Play returned wav using rodio.
7. Load preset JSON files.
8. Add daemon mode with a simple event loop.
9. Add clear errors when server is not running.
10. Keep code simple and modular.

## Do not overbuild

Do not add a database.
Do not add a web dashboard.
Do not add user accounts.
Do not add cloud services.
Do not add Docker unless asked.

## Important

Keep the voice system generic and safe. Sample voice names should be fictional or synthetic.
