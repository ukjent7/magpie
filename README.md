# magpie

One place to pick every agent's model: Codex on DeepSeek, Claude Code
on Kimi, Gemini CLI on GLM, from one native desktop app.
[usemagpie.ai](https://usemagpie.ai)

[![Discord](https://img.shields.io/badge/Discord-join%20the%20community-5865F2?logo=discord&logoColor=white)](https://discord.gg/vGSnD3ZKQF)

`magpie` lists each AI agent on your machine and the model it is set to.
Change agent settings, manage providers and routing groups, save profiles,
and review usage from the desktop app or command line.

The default command opens a native desktop window with a system tray icon.
Choose *Open magpie* from the tray menu to show it again. `magpie tray` starts
with the window hidden, `magpie tui` opens the keyboard-driven terminal UI,
and the other subcommands provide a CLI.

The terminal UI (`magpie tui`) looks like this:

```
  ◉ magpie

  ▸ Claude Code   claude-fable-5-1[1m]                        ~/.claude/settings.json
    Codex         gpt-6-astra   effort medium
    Gemini CLI    gemini-3.1-pro
    OpenCode      anthropic/claude-sonnet-5   small anthropic/claude-haiku-4-5
    Pi            openrouter/z-ai/glm-5.2:batch
    Goose         anthropic/claude-sonnet-5
    Cursor        auto
    Copilot CLI   claude-fable-5

  ↑↓ agent  ·  ←→ field  ·  ↵ change  ·  s save profile  ·  p profiles  ·  q quit
```

- **Native desktop app.** Written in Rust with egui/eframe; the interface is a
  native window, not a webview. A system tray icon is available on supported
  desktop environments.
- **Edits config files surgically.** Only the one key you change is touched;
  comments, ordering and indentation in your `settings.json`, `config.toml`,
  `opencode.jsonc` or `config.yaml` survive intact. Writes are atomic.
- **One endpoint for every agent.** magpie runs a local gateway that speaks
  OpenAI chat completions, OpenAI Responses and the Anthropic Messages API,
  and forwards to whichever vendor serves the model. Codex, Claude Code,
  OpenCode and the rest all point at `http://127.0.0.1:3425/v1` and pick
  from one catalog; the translation between APIs happens in magpie, streaming
  and tool calls included.
- **Your subscriptions, shared.** Sign in to Claude Code, Codex (ChatGPT)
  or Copilot and that login shows up as a provider: every other agent can
  use its models through the gateway, with nothing copied and no key to
  paste.
- **Providers with one field.** Pick a preset (Anthropic, OpenAI, Gemini,
  DeepSeek, Kimi, GLM, MiniMax, Qwen, Mistral, Groq, xAI, OpenRouter,
  Together, Fireworks, SiliconFlow, AiHubMix, 302.AI, Ollama, LM Studio…),
  paste a key, done. Custom vendors need a name and a base URL. magpie never
  reads keys from your shell environment.
- **Real model lists, nothing compiled in.** With a key in hand magpie asks
  the vendor which models it serves and offers exactly those; the
  [models.dev](https://models.dev) catalog fills in names, reasoning efforts
  and the list for vendors that have none, and refreshes itself in the
  background once it goes stale. Choose which models each provider exposes,
  or expose them all — a model released this morning is in the picker on
  the next refresh.
- **Profiles.** Snapshot every agent's settings under a name and switch all of
  them back in one move.
- **Groups and usage.** Combine provider models into routing groups, then
  review gateway usage by period, agent and model.
- **Five desktop pages.** Manage Agents, Providers, Profiles, Groups and Usage
  in the native app.

## Agents

| Agent        | File                              | Fields          |
| ------------ | --------------------------------- | --------------- |
| Claude Code  | `~/.claude/settings.json`         | provider, model, opus/sonnet/haiku/fable (through magpie) |
| Codex        | `~/.codex/config.toml`            | provider, model, effort |
| Gemini CLI   | `~/.gemini/settings.json`, `~/.gemini/.env` | auth, model |
| OpenCode     | `~/.config/opencode/opencode.json(c)` | model, small |
| Pi           | `~/.pi/agent/settings.json`       | model           |
| Goose        | `~/.config/goose/config.yaml`     | model           |
| Cursor CLI   | `~/.cursor/cli-config.json`       | model           |
| Copilot CLI  | `~/.copilot/settings.json`        | model           |
| Crush        | `~/.config/crush/crush.json`      | large, small    |
| DeepSeek Harness (dsh) | `~/.dsh/config.yaml` (`$DSH_HOME`) | model |
| Command Code | `~/.commandcode/settings.json` (+ `providers.json`) | model |
| omp (oh-my-pi) | `~/.omp/agent/config.yml` (+ `models.yml`) | model |
| Devin        | `~/.config/devin/config.json` (`%APPDATA%\devin\config.json` on Windows) | model |
| Hermes Agent | `~/.hermes/config.yaml` (`$HERMES_HOME`) | model |

Provider-scoped agents (OpenCode, Pi, Goose, Crush, omp, Hermes Agent) take `provider/model`.
Only agents that are installed or configured are shown.

## Providers and the gateway

Every model an agent can pick is spelled `provider/model` and served by
magpie's gateway, so agents never hold vendor keys or vendor URLs. Add a
provider, and its models appear in every agent's picker:

```sh
magpie presets                          # the vendors magpie knows, grouped: vendors, relays, local
magpie provider add deepseek sk-…       # a preset needs only the key
magpie provider add ollama              # local servers need none
magpie provider add "My Relay" url=https://relay.example.com/v1 key=sk-… models=gpt-5.5,claude-sonnet-5
magpie providers                        # host, key, exposed models, who uses what
magpie provider deepseek                # one provider in detail
magpie provider models deepseek         # re-fetch the vendor's list (add ids to choose which to expose)
magpie provider test deepseek           # one tiny request per API, with latency
magpie provider key deepseek sk-…       # replace the key
magpie provider keys deepseek            # list masked keys and their short IDs
magpie provider keys deepseek add sk-… name=Team protocol=chat
magpie provider routing deepseek rotate # spread requests over enabled keys
magpie provider affinity deepseek session # keep each conversation on its last key
magpie provider rm deepseek
magpie models                           # the catalog agents see
magpie claude deepseek/deepseek-chat    # use it
```

Custom providers take `url=` (an OpenAI-compatible base), `anthropic=` (an
Anthropic-compatible base), or both, plus `responses=` when the vendor has a
separate Responses endpoint, `catalog=` to borrow a models.dev list, and
`models=` to name the models to expose. Anything a preset does not know can
be overridden the same way.

Providers can keep several keys. `magpie provider keys <id> add <key>` adds
one; `name=` labels it, and `protocol=` can pin it to `chat`, `responses`, or
`anthropic` when a relay issues keys for separate APIs. Use the short ID shown
by `magpie provider keys <id>` with `use`, `on`, `off`, `rm`, `rename`, and
`protocol`. A key made for one API is only sent to that API, with translation
when the request protocol permits it. The provider's `routing` strategy is
`smart` (the default, configured order), `order` (same order, next key only
after a failure), `rotate` (start with the next enabled key each request), or
`usage` (prefer the key served the fewest recent requests; its count halves
each hour). A key that fails is held back for a minute, or for its numeric
`Retry-After` value up to an hour. Once every enabled key for a model has been
tried, the provider's configured model fallbacks can take over.

Conversation affinity applies to providers as well as groups. Set it with
`magpie provider affinity <id> auto|session|turn|off` (or `stays=` when adding
a provider). `auto` keeps tool calls on the same key and keeps later turns
there when the last reply read at least 1024 cached tokens within the last
five minutes; `session` keeps the whole conversation on that route, `turn`
keeps only tool-result requests there, and `off` disables stickiness.

### Routing groups

A routing group is several models, from one provider or many, that an agent
picks as one: `group/<id>`. The gateway routes each request over every
member's keys and accounts together. A model two of your providers serve
under the same name becomes a group on its own; the Groups page in the app and
`magpie group` make any other:

```sh
magpie groups                           # yours, then those magpie found
magpie group add "Opus anywhere" models=claude/claude-opus-5-5,copilot/claude-opus-5.5 routing=order stays=session
magpie group opus-anywhere              # one group, its models in order
magpie group set opus-anywhere models+=openrouter/anthropic/claude-opus-5.5 routing=usage
magpie group set opus-anywhere models-=copilot/claude-opus-5.5
magpie group rm opus-anywhere           # one magpie found is hidden; magpie group restore <id> brings it back
magpie claude group/opus-anywhere       # use it
```

`routing=` is `smart` (the default: of the subscriptions with quota to
spare, the one whose allowance renews soonest first), `order` (the first
model until it can't answer, then the next), `rotate` (each turn to the next
member) or `usage` (least used first). `stays=` controls how long a
conversation stays with the key or account that answered it: `auto` (the
default, while a warm vendor cache is worth keeping), `session`, `turn` or
`off`. Tool-result requests stay on the same route in `auto`, `session` and
`turn` modes.
`models=` replaces the whole list, in order; a bare model id works when only
one provider serves it.

The Rust CLI can import providers from Claude Code's `settings.json`
(`CLAUDE_CONFIG_DIR` when set) and Codex's `config.toml`
(`CODEX_HOME` when set) into magpie. Codex imports custom
`[model_providers.*]` entries with an inline `experimental_bearer_token`,
including fixed headers for custom providers in
`[model_providers.*.http_headers]` and models from
`[profiles.*]` or `model_catalog_json`. Review the entries before importing;
subsequent changes to agent settings are not automatically copied to magpie.
Entries that point back to magpie or only name an `env_key` are skipped.

### Signed-in agents as providers

An agent you have signed in to is a subscription with models behind it, so
magpie offers it as a provider too. Claude Code (an OAuth login in the macOS
Keychain or `~/.claude/.credentials.json`), Codex (a ChatGPT login in
`~/.codex/auth.json`), Copilot (a GitHub login in
`~/.config/github-copilot/apps.json` or the Copilot CLI's
`~/.copilot/config.json`) and Devin (`devin auth login`, kept in
`~/.local/share/devin/credentials.toml`) appear in `magpie providers` and on
the Providers page as *signed in as …*, with their models spelled
`claude/claude-sonnet-5`, `codex/gpt-5.5`, `copilot/claude-sonnet-4.5` or
`devin/swe-2-max` in every other agent's picker. magpie reads the agent's own credentials each
time, refreshes tokens the way the agent does — writing a rotated token
back where the agent will find it — and stores nothing but your model
picks; sign out of the agent and the provider is gone. The model list is
the vendor's own too: magpie asks Anthropic's, Copilot's or Codex's API with
that same sign-in, so a model added upstream appears on the next refresh.
The ChatGPT backend only streams and rejects a few parameters, so magpie
translates non-streaming requests and drops what it would refuse.
Claude subscriptions are different: Anthropic classifies another agent's
system prompt as third-party traffic even when the OAuth request otherwise
looks like Claude Code. magpie therefore drives the genuine local `claude`
binary for every Claude subscription generation. The caller's tools are
bridged into that live turn over MCP, and tool results resume the same Claude
Code process; Pi, OpenCode and every other agent use this path automatically.
The generated harness stays out of Anthropic's system-prompt classifier while
its instructions remain part of the user context. This requires Claude Code
to be installed and signed in.
Cursor, Grok and Devin subscriptions likewise run through their own CLIs —
none of them has an endpoint a borrowed key can be sent to — with Devin
driven over ACP (`devin acp`) in a home of magpie's own that keeps only the
caller's MCP tools and shares just the sign-in.
Google sign-ins — Gemini CLI's and Antigravity's — talk to Google's Code
Assist API directly: magpie reads Gemini CLI's own login from `~/.gemini` or
signs one in itself, and refreshes the token in memory. Google no longer
serves Gemini CLI's sign-in to individual accounts, only to Gemini Code
Assist Standard and Enterprise, which need a Google Cloud project named
(`magpie accounts project gemini <email> <project-id>`, or
`GOOGLE_CLOUD_PROJECT` in `~/.gemini/.env`). Google may suspend an
Antigravity account it sees used outside Antigravity, so magpie asks before
adding one; use an account you can afford to lose.

### Connecting anything else

The gateway listens on `127.0.0.1:3425` (`MAGPIE_ADDR` changes it) and starts
with the app; `magpie serve` runs it alone. It exposes:

| Path                     | API                        |
| ------------------------ | -------------------------- |
| `/v1/chat/completions`   | OpenAI chat completions    |
| `/v1/responses`          | OpenAI Responses           |
| `/v1/messages`           | Anthropic Messages         |
| `/v1/messages/count_tokens` | Anthropic token counting |
| `/v1beta/models/{model}:generateContent` | Google Gemini (also `:streamGenerateContent`, `:countTokens`) |
| `/v1/models`, `/v1beta/models` | the catalog            |

Requests pass straight through when the vendor speaks the agent's API and
are translated otherwise, streaming, tool calls and reasoning included. The
key is `magpie` (any value works; the gateway only listens on loopback), and
models are named `provider/model`. Anything with a base-URL setting can use
it:

| Tool speaks | Base URL                   | Environment                                   |
| ----------- | -------------------------- | --------------------------------------------- |
| OpenAI      | `http://127.0.0.1:3425/v1` | `OPENAI_BASE_URL`, `OPENAI_API_KEY=magpie`      |
| Anthropic   | `http://127.0.0.1:3425`    | `ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY=magpie` |
| Gemini      | `http://127.0.0.1:3425`    | `GOOGLE_GEMINI_BASE_URL`, `GEMINI_API_KEY=magpie` |

The desktop app starts the local gateway with the window. Run `magpie serve`
to start it without the app. `MAGPIE_DEBUG=1` logs gateway calls to the
terminal.

**Claude Code** gets `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN` and the
model variables in the `env` block of `settings.json`; picking a native
model (`opus`, `sonnet`…) removes them and restores whatever was there.

**Codex** gets a `[model_providers.magpie]` table, `model_catalog_json`
pointing at `~/.codex/magpie-models.json` (written from the catalog, so the
models show in Codex's own list) and a valid `model`/`effort`; picking a
native model removes all of that. Your ChatGPT sign-in is never touched.
Codex reads its model list at start-up, so restart it after a switch.

**OpenCode, Pi, Crush** get a `magpie` provider entry and `magpie/provider/model`.

**Gemini CLI** switches `auth` between API key, Google account and Vertex;
the API key goes to `~/.gemini/.env`. Picking a catalog model points
`GOOGLE_GEMINI_BASE_URL` at the gateway (which speaks the Gemini API), sets
`auth` to API key with the gateway token, and names the model in
`settings.json`; a native model puts the previous auth back.

### Import links

A vendor or relay can hand its users a ready-made provider as a link:

```
magpie://import?preset=deepseek&key=sk-…
magpie://import?name=Acme%20Relay&chat=https://api.acme.example/v1&anthropic=https://api.acme.example&key=sk-…&models=gpt-5.5,claude-sonnet-5
```

Opening one brings up magpie with what the link would add: the name, the
hosts your prompts and key would go to, the models. Nothing is saved until
you press *Add*. `magpie import <link>` does the same in a terminal.

The Rust CLI can also import explicit provider settings from Claude Code or
Codex. `magpie import apps` previews both; `magpie import apps claude` and
`magpie import apps codex` select one source. It asks before saving by
default; add `--yes` to skip the prompt. Shell environment variables are
never read as provider credentials.

| Parameter   | Meaning                                                            |
| ----------- | ------------------------------------------------------------------ |
| `preset`    | a preset id (`magpie presets`); its endpoints are used             |
| `region`    | with a preset that has regions, which one                          |
| `name`      | the provider's name; required without a preset                     |
| `id`        | its id; derived from the name when absent                          |
| `key`       | the API key; the user pastes one when absent                       |
| `chat`      | OpenAI Chat Completions base URL (`…/v1`)                          |
| `responses` | OpenAI Responses base URL (`…/v1`)                                 |
| `anthropic` | Anthropic Messages base URL (the root, without `/v1`)              |
| `models`    | model ids to expose, comma separated                               |
| `catalog`   | models.dev provider id, for model names and reasoning levels       |
| `website`, `keys` | the vendor's site and its API-key page (https)               |
| `icon`      | an https picture of the vendor's own (PNG, JPEG, GIF, WebP, ICO, SVG, at most 1 MB). magpie downloads it once, after you confirm the import, into its icons folder; without one it falls back to the catalog's logo or a plain mark |

Base URLs must be https (plain http only to this machine or the local
network). Web pages and GitHub don't link custom schemes reliably, so link
to `https://usemagpie.ai/import#<same parameters>` instead: it opens
magpie, and offers the download when it is not installed. The parameters
stay in the fragment, which browsers never send to a server. The full guide,
with a link builder: <https://usemagpie.ai/docs/import>.

## Install

Download the app for macOS, Windows or Linux from
[usemagpie.ai](https://usemagpie.ai), or install it from a terminal (on
Linux, the desktop app when WebKitGTK 4.1 is installed, the command
otherwise):

```sh
curl -fsSL https://usemagpie.ai/install.sh | sh
```

Mac releases are signed and notarised; the Windows and Linux builds are not
signed yet (Windows SmartScreen may ask before the first run). Every build
keeps itself current: the app
downloads a new version in the background and installs it when you restart
(*Restart to Update* in the menu) or quit; `magpie update` does the same from
a terminal. Every release is on
[yetone/magpie-releases](https://github.com/yetone/magpie-releases/releases).

## Rust development and CI

The Rust source uses edition 2024 and targets Rust 1.98.1. GitHub Actions is
the build and verification environment for this migration: it runs formatting,
Clippy, tests and release-mode compilation on Linux, macOS and Windows. See
[`rust.yml`](.github/workflows/rust.yml) and
[`rust-format.yml`](.github/workflows/rust-format.yml) for the workflows.

## Use

```sh
magpie                          # open the app window and system tray icon
magpie tray                     # start with the window hidden in the tray
magpie tui                      # open the keyboard-driven terminal UI
magpie ls                       # list every agent and its current settings
magpie claude opus              # set a model (agent names accept prefixes: cc, oc, gem …)
magpie codex gpt-5.6-sol
magpie codex effort high        # other fields
magpie codex xhigh              # bare effort levels are recognised too
magpie codex deepseek/deepseek-chat   # any catalog model, through the gateway
magpie claude moonshot/kimi-k2.5
magpie claude haiku deepseek/deepseek-v4-flash   # one tier on its own model
magpie claude haiku ""          # back to the main model
magpie gemini auth api-key
magpie opencode anthropic/claude-sonnet-5
magpie oc small anthropic/claude-haiku-4-5

magpie save work                # snapshot everything as a profile
magpie use work                 # switch back
magpie profiles
magpie rm work

magpie sync                     # refresh the models.dev catalog and every live model list
```

The desktop app has Agents, Providers, Profiles, Groups and Usage pages. Use
them to edit agent settings, manage provider keys and model exposure, configure
routing, switch saved profiles, and inspect usage. The keyboard-driven
terminal interface is available separately with `magpie tui`.

Keys in the terminal version:

| Key        | Action                                |
| ---------- | ------------------------------------- |
| `↑` `↓`    | choose agent                          |
| `←` `→`    | choose field (model, effort, small …) |
| `↵`        | open the picker                       |
| type       | filter; enter accepts custom values   |
| `s`        | save current setup as a profile       |
| `p`        | apply or delete (`ctrl+d`) a profile  |
| `S`        | sync the model catalog                |
| `q`        | quit                                  |

Agents read their config at startup, so a running session keeps its model
until you start a new one.

### Moving to another machine

```sh
magpie backup                   # writes magpie.magpie-backup, asks for a passphrase twice
magpie backup --no-keys ~/b.magpie-backup   # the same with no API keys in it
magpie restore magpie.magpie-backup         # on the other machine
magpie restore --no-agents b.magpie-backup  # providers, settings, profiles; agents left as they are
```

A backup holds your providers (with their keys, unless `--no-keys`), the
pictures picked for them, the settings, the profiles and every agent's model.
It is encrypted on your machine (AES-256-GCM, the key derived from the
passphrase with PBKDF2-SHA256); nothing in it can be read without the
passphrase. Restoring replaces providers with the same id and adds the rest;
one that came without a key keeps the key already there. Agent models are set
only for agents installed on that machine. Subscriptions are not in it: sign
in to them on each machine. Piped in, the passphrase is the first line of
stdin.

## Files

- `~/.config/magpie/profiles.json` — saved profiles
- `~/.config/magpie/providers.json` — your providers, keys included (0600)
- `~/.config/magpie/stash.json` — values magpie replaced, restored on switch-back
- `~/.cache/magpie/models.json` — models.dev catalog (OpenCode's cache at
  `~/.cache/opencode/models.json` is used when present)
- `~/.cache/magpie/models/<provider>.json` — model lists fetched from vendors

`XDG_CONFIG_HOME` and `XDG_CACHE_HOME` are respected.

## Community

Questions, setups worth sharing, ideas, bugs: come talk to us and other
magpie users on [Discord](https://discord.gg/vGSnD3ZKQF). Issues and pull
requests are welcome here too.

## License

MIT. See [LICENSE](LICENSE).
