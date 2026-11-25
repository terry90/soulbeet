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

The pictures in [screenshots](../screenshots) are taken on the e2e stub stack, so they need no running Soulbeet, no account and no real service. The specs register their own account, search the stub MusicBrainz for "Night Garden" (fixture albums kept for the pictures) and stage the rest in the browser: drawn covers, the downloads panel part way through an album, a Navidrome library for the dashboard and its health, two folders, and the version check the app fetches from the internet. The downloader and beets report their real health.

1.  Build the app (the stack runs the release binary) from the repository root:
    ```bash
    npm ci && npx @tailwindcss/cli -i ./web/assets/input.css -o ./web/assets/tailwind.css
    dx bundle --package web --release
    ```
2.  Start the stub stack and take the pictures:
    ```bash
    cd e2e && npm ci && npx playwright install chromium
    scripts/dev-stack.sh up
    npm run screenshots
    scripts/dev-stack.sh down
    ```
    The stack needs a Python 3.11 interpreter, as in the image (`PYTHON=/path/to/python3.11` if it is not `python3.11` on the PATH).
