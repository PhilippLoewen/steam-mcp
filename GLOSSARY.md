# steam-mcp

An MCP server that lets an AI assistant read and manage a user's local Steam install. Every tool argument comes from the model, so the model may have been steered by text it read elsewhere.

## Language

### Actions and modes

**Action**:
One operation of a tool, selected by its `action` argument; a tool groups related actions.
_Avoid_: command, subcommand, operation

**Write action**:
An action that changes a file, changes Steam's state, or starts a program. Opening a store page is not one.
_Avoid_: mutation, destructive action

**Read-only mode**:
The server state in which write actions are hidden from the model and refused; the user turns it on in the MCP client's configuration.
_Avoid_: safe mode, dry run

### Steam

**Active account**:
The Steam account signed in most recently on this computer; every edit of per-account data goes to it.
_Avoid_: current user, logged-in user

**Category**:
A user-made or built-in group of games in the Steam library, such as Favorites or Hidden.
_Avoid_: collection, tag

**Millennium bridge**:
The plugin inside the running Steam client through which categories are edited while Steam runs.

### Files the server produces

**Output folder**:
The one folder the server writes exports and backups into, `steam-mcp` inside the user's Documents unless the user configures another. The model names files inside it and cannot leave it.
_Avoid_: export dir, backup root, destination

**Export**:
A new JSON or CSV file in the output folder listing the library. It never replaces an existing file.

**Backup**:
A new, timestamped folder in the output folder holding copies of local cloud saves. It never merges into an existing folder.
_Avoid_: snapshot, save copy

### Launching games

**Launch options**:
The per-game text Steam adds when it starts a game: arguments for the game, or settings and wrapper programs placed around it.
_Avoid_: launch args, command line

**Preset**:
A named, built-in set of launch options that is merged into a game's existing ones.

**Shortcut**:
A non-Steam program added to the Steam library, with its own launch options.
_Avoid_: non-Steam game, custom game
