# Development

This page covers running Soulbeet from source, running the tests and refreshing the screenshots.

## Running from source

1.  Install Rust and `dioxus_cli`.
2.  Run the tailwind watcher:
    ```bash
    ./css.sh
    ```
3.  Run the app:
    ```bash
    dx serve --platform web
    ```
4.  Run the tests (the Python tests need Python 3.11+ with beets and pytest installed):
    ```bash
    cargo test --workspace --features server,dioxus/server --lib
    cargo test -p soulbeet --lib
    python -m pytest -q lib/soulbeet/tests/probe lib/soulbeet/tests/import
    ```

## Refreshing the screenshots

The pictures in [screenshots](../screenshots) are taken on a running Soulbeet, so they show a real library. Nothing is downloaded or changed there: the downloads panel is staged, it shows the album you searched for part way through without fetching it.

```bash
cd e2e && npm ci && npx playwright install chromium
E2E_BASE_URL=https://music.example.com SHOTS_USER=you SHOTS_PASSWORD=... SHOTS_QUERY="an album title" npm run screenshots
```
