# Focus d’une session dans son terminal

Investigation du 5 octobre 2026. Un clic sur une carte (ou une notification,
ou une demande IPC) doit ramener au premier plan **l’onglet** qui héberge la
session, pas seulement l’application. Ce document recense ce que chaque
terminal permet, ce qu’AgentsMon fait aujourd’hui et les pistes écartées.

## Chaîne commune

1. Le helper (`crates/agentsmon-helper/src/record.rs`, `EnvironmentMetadata`)
   lit l’environnement du hook : `host_app` (`LC_TERMINAL` / `TERM_PROGRAM`),
   `terminal_session_id`, `TMUX_PANE` / `TMUX`. Il n’enregistre que ces
   identifiants, jamais de contenu.
2. Le provider construit un `FocusTarget` ; `find_host` remonte l’arbre des
   processus pour trouver le PID de l’hôte.
3. `crates/agentsmon-providers/src/focus.rs`, `focus()` : une URI de provider
   (`FocusTarget::uri` : VS Code, Cursor, Claude.app, Codex) passe en premier,
   puis tmux (`select-window` + `select-pane`) si présent, puis `focus_host` :
   sélection spécifique à l’hôte (iTerm2, puis Warp) et, en dernier recours,
   activation de l’application par PID (`focus_pid`).

Pour qu’un terminal soit ciblable onglet par onglet, il faut **les deux** :
un identifiant par onglet/pane visible dans l’environnement du shell, et une
API externe qui sélectionne cet identifiant. Un ciblage par répertoire de
travail est ambigu (deux sessions dans `~/mass/mass-api`, par exemple).

## Synthèse

| Terminal | Identifiant dans l’env | API de sélection | Onglets verticaux | Fenêtre type « quake » | État AgentsMon |
|---|---|---|---|---|---|
| Warp | `WARP_TERMINAL_SESSION_UUID`, `WARP_FOCUS_URL` | `warp://session/<uuid>` | ✅ | ✅ (hotkey dédiée) | ✅ onglet |
| iTerm2 | `ITERM_SESSION_ID` | AppleScript (`select`, `reveal hotkey window`) | ✅ (barre à gauche) | ✅ (hotkey window) | ✅ onglet, sauf hotkey window cachée |
| cmux | `CMUX_WORKSPACE_ID`, `CMUX_SURFACE_ID` | CLI / socket `cmux select-workspace`, `focus-panel` | ✅ | ⚠️ show/hide global | ❌ app seulement |
| Ghostty 1.3 | aucun par terminal | AppleScript (`focus`) | ❌ | ✅ quick terminal | ❌ app seulement |
| WezTerm | `WEZTERM_PANE` | `wezterm cli activate-pane` | ❌ | ❌ | ❌ app seulement |
| kitty | `KITTY_WINDOW_ID` | `kitten @ focus-window` (remote control) | ❌ | ✅ quick-access-terminal | ❌ app seulement |
| Terminal.app | `TERM_SESSION_ID` | AppleScript (onglets par `tty`) | ❌ | ❌ | ❌ app seulement |
| VS Code / Cursor | — | `vscode://file/…`, `cursor://file/…` | — | — | dossier |
| Windows Terminal | `WT_SESSION` | aucune | ❌ | ✅ | fenêtre seulement |

## Warp (implémenté)

- Warp n’a pas de dictionnaire AppleScript. Son arbre d’accessibilité ne contient
  qu’une zone de texte et les boutons de fenêtre, sans les onglets.
- Depuis la résolution de [warpdotdev/warp#8611](https://github.com/warpdotdev/warp/issues/8611)
  (fermée comme *completed* le 22 mai 2026), chaque session exporte
  `WARP_TERMINAL_SESSION_UUID` (32 hexadécimaux) et
  `WARP_FOCUS_URL=warp://session/<uuid>`. Observé avec
  `WarpTerminal v0.2026.09.09.08.26.stable_02`.
- Le helper enregistre l’UUID comme `terminal_session_id` **seulement si
  `TERM_PROGRAM=WarpTerminal`** : sous tmux, ou dans une application lancée
  depuis Warp qui hérite des variables, l’UUID ne désigne pas l’hôte réel.
- `focus_host` construit `warp://session/<uuid>` lui-même (identifiant restreint
  aux hexadécimaux et `-`, 64 caractères au plus) au lieu d’ouvrir une URL lue
  dans un fichier. En cas d’échec, il retombe sur l’activation par PID.
- Les sessions démarrées avant la mise à jour du helper n’ont pas d’UUID : leur
  prochain hook l’ajoute.
- Piste écartée : `warp.sqlite` (Group Container `2BBY89MBSN.dev.warp`) contient
  bien `windows` / `tabs` / `terminal_panes`, mais c’est un instantané de
  restauration (4 onglets anciens contre 11 ouverts lors du test). La base contient
  aussi l’historique des blocs, donc des commandes et leurs sorties : ne pas la lire.
- Piste écartée : simuler `Cmd+N` ou la touche de la hotkey. Il n’y a pas d’index
  d’onglet fiable, et la hotkey bascule (elle cacherait une fenêtre déjà visible).

Vérification manuelle : depuis un autre onglet Warp,
`sleep 5; open "$WARP_FOCUS_URL_DE_LA_SESSION"`, puis cacher Warp avec la hotkey.
Warp doit réapparaître sur le bon onglet.

## iTerm2 (implémenté, à compléter)

- `ITERM_SESSION_ID` (`w0t1p0:<uuid>`) est capturé. Le script AppleScript de
  `focus_iterm_session` parcourt `windows → tabs → sessions`, compare
  `unique ID`, puis appelle `select` sur l’onglet et la fenêtre et `activate`.
- La barre d’onglets peut être placée à gauche (Settings → Appearance →
  Tab bar location), et la « hotkey window » dédiée accepte des onglets.
- Reste à faire : une hotkey window cachée n’est pas révélée par `activate`. Le
  dictionnaire (`iTerm2.sdef`) expose `is hotkey window` sur la fenêtre et la
  commande `reveal hotkey window`, qu’il faudrait appeler dans ce cas.

## cmux (non implémenté, meilleur candidat alternatif)

- [cmux](https://github.com/manaflow-ai/cmux) est un terminal macOS basé sur
  Ghostty, avec une sidebar verticale pensée pour les agents.
- Chaque terminal exporte `CMUX_WORKSPACE_ID` et `CMUX_SURFACE_ID`. La
  [CLI](https://cmux.com/docs/api) (socket `/tmp/cmux.sock`, ou
  `CMUX_SOCKET_PATH`) propose `select-workspace --workspace <id>` et
  `focus-panel --panel <id>`.
- La documentation ne dit pas si ces commandes ramènent la fenêtre au premier
  plan : prévoir `focus_pid` ensuite, comme pour iTerm2.
- Pas de vrai mode « quake ». Il existe un raccourci global
  `showHideAllWindows` ([configuration](https://cmux.com/docs/configuration)).
- Implémentation : capturer `CMUX_SURFACE_ID` (et le workspace) dans le helper,
  puis ajouter une branche cmux dans `focus_host` qui appelle la CLI avec un
  identifiant validé.

## Ghostty 1.3 (non implémenté)

- Le [support AppleScript](https://ghostty.org/docs/features/applescript)
  (`Ghostty.sdef`, 1.3.0) expose `windows → tabs → terminals`, avec pour chaque
  terminal `id`, `name` et `working directory`, ainsi que les commandes `focus`,
  `select tab` et `activate window`. Il enveloppe les App Intents (Raccourcis).
- Bloquant : aucune variable d’environnement n’identifie le terminal (le binaire
  ne connaît que `GHOSTTY_RESOURCES_DIR`, `GHOSTTY_BIN_DIR`,
  `GHOSTTY_SHELL_FEATURES`, `GHOSTTY_QUICK_TERMINAL`…). Seul un ciblage par
  `working directory` est possible, ambigu quand plusieurs sessions partagent
  un dossier. L’`id` AppleScript n’est pas visible depuis le shell.
- Pas d’onglets verticaux ([discussion #2549](https://github.com/ghostty-org/ghostty/discussions/2549)).
  Le quick terminal n’est pas exposé par AppleScript.

## WezTerm, kitty, Terminal.app (non implémentés)

Lignes tirées de la documentation de chaque projet, non vérifiées sur une
machine (aucun des trois n’était installé lors de l’investigation).

- WezTerm : `WEZTERM_PANE` puis `wezterm cli activate-pane --pane-id <id>`.
  Ciblage fiable, mais ni onglets verticaux natifs ni fenêtre quake.
- kitty : `KITTY_WINDOW_ID` puis `kitten @ focus-window --match id:<id>`.
  Nécessite `allow_remote_control` et un `listen_on` connu d’AgentsMon. Le
  kitten `quick-access-terminal` fournit le mode quake. Pas d’onglets verticaux.
- Terminal.app : le dictionnaire AppleScript expose le `tty` de chaque onglet.
  Le helper devrait enregistrer le tty du processus Claude.

## Contraintes

- Ne jamais lire de contenu de conversation ni de sortie de terminal pour
  retrouver un onglet (titres de blocs, historique, bases de restauration).
- Toute valeur issue d’un fichier de statut et passée à `open`, `osascript` ou
  une CLI doit être validée (alphabet restreint, longueur bornée) et transmise
  en argument, jamais interpolée dans un script.
- Une IPC ne marque une session comme lue qu’après un focus réussi
  (`focus_agent_result`). Un clic utilisateur la marque comme lue même si
  le focus échoue.
