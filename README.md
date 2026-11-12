# Steam MCP Server

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

A feature-rich [Model Context Protocol](https://modelcontextprotocol.io/) server that gives AI assistants full access to your Steam game library. Works on **Linux, macOS, and Windows**. 14 consolidated tools covering 66 actions — most work from local files with **no API key needed**. Optional Steam Web API key unlocks online data like achievements, friends, and full library access.

Works with Claude Desktop, Claude Code, or any MCP-compatible client.

## What can it do?

**Ask your AI assistant things like:**

- "What games do I have installed?"
- "Search for ck3" → finds Crusader Kings III via abbreviation matching
- "How much disk space is Steam using?" → full breakdown by game
- "Find orphaned data I can clean up" → identifies leftover compatdata/shadercache
- "What Proton version should I use for this game?"
- "Install Hollow Knight" → looks up the appid and tells Steam to install it
- "Pick a random game for me to play"
- "Show my full play history" → all-time playtime across your entire library
- "Import my Lutris games as Steam shortcuts"
- "Show my achievements for Hades" → unlock status, completion %, global stats
- "Who's online right now?" → friends list with online status and current games
- "What's on my wishlist?"
- "Show me reviews for Elden Ring"
- "How long to beat Hollow Knight?"

## Installation

steam-mcp is a single native binary written in Rust. Build and install it with
[Cargo](https://rustup.rs/) (Rust 1.88+):

```bash
cargo install --git https://github.com/Praeses0/steam-mcp.git
```

or from a checkout:

```bash
cargo install --path .
```

This puts `steam-mcp` in `~/.cargo/bin`. Use that as the command in your MCP client
config (or its absolute path if `~/.cargo/bin` isn't on the client's `PATH`).

### Configure with Claude Desktop

Add to your Claude Desktop config:
- **Linux**: `~/.config/Claude/claude_desktop_config.json`
- **macOS**: `~/Library/Application Support/Claude/claude_desktop_config.json`
- **Windows**: `%APPDATA%\Claude\claude_desktop_config.json`

```json
{
  "mcpServers": {
    "steam": {
      "command": "steam-mcp",
      "env": {
        "STEAM_API_KEY": "your-key-here"
      }
    }
  }
}
```

### Configure with Claude Code

Add a `.mcp.json` in your project root, or add to `~/.claude.json` for system-wide access:

```json
{
  "mcpServers": {
    "steam": {
      "command": "steam-mcp",
      "env": {
        "STEAM_API_KEY": "your-key-here"
      }
    }
  }
}
```

> **Note:** The `STEAM_API_KEY` is optional. Most tools work without it. Only a few actions (achievements, friends, owned games, profile) require it. The wishlist needs no key, but it must be public. Get your key at [steamcommunity.com/dev/apikey](https://steamcommunity.com/dev/apikey).

### Category filtering

To reduce context window usage further, enable only specific tool categories via the `STEAM_TOOLS` env var:

```json
{
  "env": {
    "STEAM_TOOLS": "core,manage,compat"
  }
}
```

Available categories and their tools:

| Category | Tools | Description |
|----------|-------|-------------|
| `core` | `games`, `library`, `steam_status` | Game browsing, library stats, Steam status |
| `compat` | `proton`, `diagnostics` | Proton/Wine compatibility, log parsing |
| `manage` | `game_control`, `game_config`, `shortcuts` | Install/launch/verify, launch options, non-Steam shortcuts |
| `storage` | `storage` | Disk usage, cleanup, shader cache, cloud saves, backups |
| `api` | `player`, `workshop` | Player profile/achievements/friends, workshop items |
| `market` | `deals` | Sale checks, wishlist, library valuation |
| `insights` | `insights` | Completion stats, timeline, year review, play history, export |
| `content` | `game_info` | Reviews, news, media, HLTB, PCGamingWiki fixes |

When unset, all categories are enabled (14 tools, ~3,200 tokens of context).

### Read-only mode

Several actions change your system: they edit Steam's config files, set launch options that run with your games, start installs or games, or write exports and backups. An assistant that reads text from the web (game news, reviews, workshop descriptions, PCGamingWiki) can be steered by instructions hidden in that text, so you may want to switch these actions off:

```json
{
  "env": {
    "STEAM_MCP_READONLY": "1"
  }
}
```

Any non-empty value except `0`, `false` or `no` (in any case) turns read-only mode on. The write actions are then left out of the tool list and refused if called anyway. They are:
- `set_launch_opts`, applying a preset and MangoHud `set`/`reset`
- the category edits
- shortcuts `add`, `remove` and `import_lutris`
- storage `backup` and insights `export`
- game_control `install`, `uninstall`, `launch`, `verify` and `move`

Every tool also carries MCP annotations (`readOnlyHint` and `openWorldHint`; tools that write also `destructiveHint`), so clients that support them can ask before a write.

### Output folder

`insights export` and `storage backup` write only inside one folder: `steam-mcp` in your Documents folder (`XDG_DOCUMENTS_DIR` on Linux, `~/Documents` on macOS and Windows). It is created the first time it's needed. Set `STEAM_MCP_OUTPUT_DIR` to an absolute path to use another folder. Tools only choose file names inside it, and nothing there is ever overwritten: an export refuses a name that is taken, and every backup goes into a new `saves-<time>` folder.

> **Breaking change after 2.0.0:** backup no longer takes a `destination`, and export's `output` is now an optional file name inside the output folder rather than a path.

### What the tools may write

Launch options run through a shell when Steam starts the game, so `set_launch_opts` and new shortcuts only accept:
- game arguments (`-novid -high`), or
- `[VAR=value …] [wrapper …] %command% [game arguments …]`

Wrappers are:
- `mangohud`, optionally with `--dlsym`
- `gamemoderun`, `prime-run` and `obs-gamecapture`
- `strangle`, with a frame-rate limit
- `gamescope`, with its size, refresh, scaling and display options, then `--`

Variables must start with:
- `PROTON_`, `DXVK_`, `VKD3D_`, `WINE_FULLSCREEN_`
- `MANGOHUD`
- `__NV_`, `__GLX_`, `__GL_`
- `SDL_`, `STEAM_LINUX_RUNTIME_`, `RADV_`
- `AMD_VULKAN_ICD`, `ENABLE_VKBASALT`

Variable values can't contain paths. Variables that run a program or load a file are refused, such as `PROTON_REMOTE_DEBUG_CMD` and `MANGOHUD_CONFIGFILE`, and `MANGOHUD_CONFIG` follows the same rules as MangoHud `set` below. Quotes, `;`, `|`, `&`, `$`, backticks, redirections and anything else a shell would interpret are refused. For other launch options, set them in Steam's game properties yourself. Presets are built in and skip these checks; they are merged into the game's existing launch options.

For `shortcuts add`, the program must be an existing executable, given by absolute path; a symlink is saved as the file it points to. On Linux and macOS, neither the program nor its start folder may be somewhere anyone can write to, such as `/tmp`. The program can't be a shell, interpreter, `wine`, terminal or other program that runs whatever its arguments say; this list is best effort, not a guarantee. On Windows, only `.exe` programs outside the temp folder are accepted. `import_lutris` doesn't take a program from the model: every imported shortcut runs `lutris` with the game's Lutris id.

MangoHud `set` refuses the keys that run commands, write files or upload logs (`exec`, `output_folder`, `output_file`, `control`, `upload_log`, `upload_logs`, `permit_upload`). It works on the global config only, because MangoHud finds per-game configs by program name, not by Steam appid.

Edits to `localconfig.vdf`, `shortcuts.vdf` and MangoHud configs are atomic. The previous version is kept as `<file>.<time>.bak`, and the newest five backups are kept. A `localconfig.vdf` or `shortcuts.vdf` that can't be read back exactly (damaged, cut off, not UTF-8) is left alone.

Launch options, presets and shortcuts can only be changed while Steam is closed, because Steam rewrites those files from memory when it exits. Category edits while Steam runs need the [Millennium bridge](#editing-categories-while-steam-runs). To tell whether Steam runs, steam-mcp uses `pgrep` on Linux and macOS and `tasklist` on Windows. If that check fails, these edits are refused as if Steam were running. steam-mcp has to see Steam's processes: in a sandbox or container with its own process list, Steam looks closed and these edits are not refused.

### Prerequisites

- Steam installed in the default location:
  - **Linux**: `~/.local/share/Steam/`
  - **macOS**: `~/Library/Application Support/Steam/`
  - **Windows**: `C:\Program Files (x86)\Steam\`

> Proton/Wine tools (compatibility tracking, prefix inspection, crash logs) are Linux-only. All other tools work cross-platform.

## Tools

Each tool uses an `action` parameter to select the operation. 14 tools cover 66 actions total.

### `games` — Game discovery and browsing
| Action | Description |
|--------|-------------|
| `list` | List installed games with filtering, sorting, pagination |
| `get` | Get full details for a game by appid |
| `search` | Smart search with abbreviation matching (e.g. "ck3" → Crusader Kings III) |
| `compare` | Compare two games side-by-side |
| `random` | Pick a random installed game |
| `unplayed` | Find games you own but haven't played |

### `library` — Library folders, stats and categories
| Action | Description |
|--------|-------------|
| `list` | List all Steam library folders with sizes and free space |
| `stats` | Aggregate stats: total games, playtime, disk usage |
| `categories` | List library categories (Favorites, Hidden, tag and custom collections) |
| `category` | Show one category's games (`name`) |
| `category_add` | Add games to a category (`name`, `appids`); Favorites/Hidden are created if missing |
| `category_remove` | Remove games from a category (`name`, `appids`) |
| `category_create` | Create an empty custom category (`name`) |
| `category_rename` | Rename a category (`name`, `newName`) |
| `category_delete` | Delete a category (`name`) |

While Steam is **running**, category edits go through the [steam-mcp-bridge Millennium plugin](#editing-categories-while-steam-runs) when it is installed (the result then says `"via": "millennium"`); without it they are refused. While Steam is **closed**, edits write `userdata/<id>/config/cloudstorage/cloud-storage-namespace-1.json` (the old version is kept as `.bak`) and list the changed key in `cloud-storage-namespace-1.modified.json`, so Steam uploads it to the cloud on its next start. Edits go to the signed-in account's file only and are refused if it has none yet. Dynamic (filter-based) categories can be renamed or deleted, but their games can't be edited by hand.

#### Editing categories while Steam runs

Steam keeps categories in memory, so editing the file under a running Steam doesn't stick. With [Millennium](https://steambrew.app/) (v3.4+) installed, the bundled `millennium-plugin/` makes the edit inside the Steam client through its own collection store. Steam then saves and cloud-syncs it as if you had made it in the library.

```bash
cd millennium-plugin
bun install
bun run build      # packs steam-mcp-bridge.star into the Millennium plugins folder
```

Then enable **Steam MCP Bridge** in Millennium's plugin settings. steam-mcp talks to Millennium over `/tmp/millennium-mep.sock`; set `MILLENNIUM_MEP_SOCKET` if yours lives elsewhere (e.g. when the MCP server runs in a sandbox with a private `/tmp`). The socket must belong to your user and be served by a process of your user; otherwise the bridge isn't used. The bridge works on Linux and macOS; on Windows, category edits need Steam closed. Live edits only cover games Steam knows in this library, and renaming relies on a Steam client internal, so it reports an error if a Steam update breaks it.

### `steam_status` — Steam client status
| Action | Description |
|--------|-------------|
| `status` | Check if Steam is running (`steamRunning` is `null` when that can't be checked) |
| `queue` | View current download queue |
| `progress` | Check download progress for a game |

### `proton` — Proton/Wine compatibility (Linux)
| Action | Description |
|--------|-------------|
| `list_games` | List all games using Proton/Wine with versions and prefix sizes |
| `info` | Get compatibility details for a specific game |
| `versions` | List installed Proton versions and which games use them |
| `db_rating` | Fetch ProtonDB community rating |
| `recommend` | Get a Proton version recommendation for a game |
| `prefix` | Inspect Wine/Proton prefix (Windows version, file count, size) |
| `crash_logs` | Find and read crash dumps and logs |

### `storage` — Disk management
| Action | Description |
|--------|-------------|
| `disk_report` | Per-game disk usage breakdown (install + compat + shader + workshop) |
| `orphaned` | Find leftover data from uninstalled games |
| `cleanup` | Get cleanup recommendations with space savings |
| `shader_cache` | Shader cache size for a specific game |
| `shader_stats` | Shader cache overview across all games |
| `saves_list` | List local cloud save data per game |
| `saves_stats` | Cloud save aggregate stats |
| `backup` | Back up cloud saves into a new folder in the [output folder](#output-folder) |

### `game_control` — Game actions
| Action | Description |
|--------|-------------|
| `install` | Install a game via Steam |
| `uninstall` | Uninstall a game |
| `launch` | Launch a game |
| `verify` | Verify game file integrity |
| `move` | Move a game to another library folder |
| `store_page` | Open the Steam store page |

### `game_config` — Configuration and tweaks
| Action | Description |
|--------|-------------|
| `get_launch_opts` | Read current launch options |
| `set_launch_opts` | Set launch options ([accepted forms](#what-the-tools-may-write)) |
| `presets` | List/apply launch option presets (MangoHud, GameMode, Vulkan, etc.) |
| `mangohud` | Show, set, or reset the global MangoHud configuration |
| `steam_settings` | View Steam client settings |

### `player` — Player data (most actions require API key)
| Action | Description |
|--------|-------------|
| `summary` | Player profile with online status and account info |
| `level` | Steam level, XP, and badges |
| `bans` | VAC and game ban status |
| `friends` | Friends list with online status and current games |
| `achievements` | Per-game achievement unlock status |
| `global_achievements` | Global achievement unlock percentages |
| `schema` | Achievement and stat definitions for a game |
| `owned` | Complete owned games list with playtime |
| `recent` | Recently played games |

### `workshop` — Steam Workshop
| Action | Description |
|--------|-------------|
| `list` | List installed workshop items for a game |
| `stats` | Workshop stats across all games |
| `search` | Search the Steam Workshop |

### `shortcuts` — Non-Steam game shortcuts
| Action | Description |
|--------|-------------|
| `list` | List all non-Steam shortcuts |
| `add` | Add a new shortcut |
| `remove` | Remove a shortcut |
| `import_lutris` | Import games from Lutris |

### `deals` — Pricing and wishlist
| Action | Description |
|--------|-------------|
| `check_sale` | Check if a game is on sale |
| `wishlist_deals` | Find discounted games on your wishlist |
| `wishlist` | View full wishlist with prices and reviews |
| `library_value` | Estimate total library value |

### `insights` — Analytics and history
| Action | Description |
|--------|-------------|
| `completion` | Achievement completion rates across top games |
| `timeline` | Month-by-month gaming activity |
| `year_review` | Annual gaming recap |
| `play_history` | Full playtime history (installed + uninstalled games) |
| `export` | Export library data as JSON or CSV into the [output folder](#output-folder) |

### `game_info` — Game content and metadata
| Action | Description |
|--------|-------------|
| `reviews` | User reviews with scores |
| `news` | Latest news articles |
| `media` | Game artwork URLs and local overrides |
| `hltb` | HowLongToBeat completion time estimates |
| `pcgamingwiki` | PCGamingWiki fixes and tweaks |

### `diagnostics` — Log parsing
| Action | Description |
|--------|-------------|
| `compat_log` | Parse Proton/Wine compatibility logs |
| `shader_log` | Parse shader compilation logs |

## Key Highlights

- **Minimal context footprint** — 14 tools, ~3,200 tokens (down from 66 tools / ~8,000 tokens in v1)
- **Cross-platform** — Linux, macOS, and Windows (Proton tools are Linux-only)
- **No API key needed** for most actions — reads directly from Steam's local files
- **Smart search** — "ck3" finds "Crusader Kings III", "ror2" finds "Risk of Rain 2"
- **Full play history** — playtime for every game you've ever played, not just installed ones
- **Storage analysis** — shows exactly where disk space goes (installs + compatdata + shadercache + workshop)
- **Orphan detection** — finds GBs of leftover data from uninstalled games
- **Multi-library** — supports multiple Steam library folders including external drives
- **Category filtering** — enable only the tool groups you need via `STEAM_TOOLS` env var
- **Custom VDF parsers** — both text and binary formats, no external dependencies
- **Single native binary** — no Node.js runtime; starts instantly

## Development

```bash
git clone https://github.com/Praeses0/steam-mcp.git
cd steam-mcp
cargo build --release   # binary at target/release/steam-mcp
cargo test              # unit tests
cargo clippy --all-targets
```

## Architecture

```
src/
├── main.rs                  # Entry point: stdio transport, STEAM_TOOLS filtering
├── server.rs                # MCP handler: tools/list and tools/call, read-only mode, annotations
├── tool.rs                  # Tool/parameter specs, JSON Schema, argument validation, write marks
├── output.rs                # The output folder for exports and backups
├── util.rs                  # Formatting, fs helpers (atomic writes, backups), FileCache
├── vdf/
│   ├── text.rs              # Text VDF recursive descent parser + serializer
│   └── binary.rs            # Binary VDF parser + writer (shortcuts.vdf)
├── steam/
│   ├── paths.rs             # Cross-platform Steam dir resolution, user detection
│   ├── manifests.rs         # App manifest reading with mtime caching
│   ├── library.rs           # Library folder operations + disk info
│   ├── userdata.rs          # localconfig.vdf read/write
│   ├── compat.rs            # Proton/Wine compatibility tracking
│   ├── launch_options.rs    # What launch options and shortcut programs may be set
│   ├── workshop.rs          # Workshop manifest reading
│   ├── shortcuts.rs         # Binary shortcuts.vdf read/write
│   ├── categories.rs        # Library categories (cloud-storage-namespace-1.json) read/write
│   ├── millennium.rs        # Live category edits via the Millennium bridge plugin
│   ├── api.rs               # Steam Web API / Store API client
│   └── wishlist.rs          # Wishlist, store details and prices
└── tools/                   # 14 consolidated MCP tools (one file per tool)
millennium-plugin/           # steam-mcp-bridge: Millennium plugin for live category edits
```

## License

[MIT](LICENSE)
