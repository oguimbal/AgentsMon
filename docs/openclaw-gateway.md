# OpenClaw : suivi des sessions par la Gateway

AgentsMon suit les sessions OpenClaw en **lecture seule** au travers du
WebSocket de la Gateway (protocole v4), généralement distante. Référence :
dépôt `openclaw/openclaw`, version 2026.9.7 (commit `e1b701b4`, 30 septembre
2026), `docs/gateway/clients.md`, `docs/gateway/protocol/*.md` et
`packages/gateway-protocol`. Le protocole évolue vite : relire ces sources
avant de toucher au client et ne pas accepter une autre version de protocole
sans tests.

## Connexion et appairage

1. Clic droit sur les icônes des providers → « Connecter OpenClaw… » (proposé
   tant qu'OpenClaw n'a pas d'identifiant valide : jamais configuré, refusé ou
   appairage en attente ; une simple coupure réseau ne compte pas).
2. L'utilisateur saisit l'URL de l'interface web (`http(s)://hote:18789/`,
   chemin de base compris) et le **token partagé** de la Gateway. Ce token reste
   en mémoire le temps de l'appairage et n'est jamais écrit sur disque.
3. AgentsMon signe le défi `connect.challenge` avec sa clé Ed25519 (charge
   `v3|deviceId|cli|cli|operator|operator.read|signedAt|token|nonce|darwin|`),
   `client.id = "cli"`, `mode = "cli"`, `displayName = "AgentsMon"`.
4. Une Gateway distante répond `PAIRING_REQUIRED` avec un `requestId`. La
   fenêtre affiche `openclaw devices approve <requestId>` et réessaie toutes
   les 3 s (la Gateway garde le même `requestId` tant que clé, rôle et scopes
   ne changent pas), sauf si `pauseReconnect` est vrai. Abandon après 15 min.
5. Une fois approuvé, `hello-ok.auth.deviceToken` est enregistré ; sans lui,
   l'appairage échoue (`no-device-token`), car le token partagé n'est pas
   conservé. L'URL est alors écrite dans `openclaw.json`.

Fichiers du dossier de données (`~/Library/Application Support/com.oliv.AgentsMon`) :

| Fichier | Contenu | Droits |
| --- | --- | --- |
| `openclaw.json` | `{"url": …}`, éventuellement `recentHours` ; un `token`/`password` écrit à la main est accepté | 0600 |
| `openclaw-device.json` | clé secrète Ed25519 et jetons d'appareil par origine de Gateway | 0600 |

`OPENCLAW_GATEWAY_URL`, `OPENCLAW_GATEWAY_TOKEN` et `OPENCLAW_GATEWAY_PASSWORD`
remplacent le fichier. Supprimer `openclaw.json` désactive le provider ;
supprimer `openclaw-device.json` impose un nouvel appairage (révoquer l'ancien
appareil avec `openclaw devices`).

## Ce qui est lu, et ce qui ne l'est jamais

- Scope demandé : `operator.read` seulement. Capacité annoncée :
  `session-scoped-events`, pour que les flux `chat`/`agent`/`session.tool` ne
  soient jamais poussés. Aucun abonnement (`sessions.subscribe` enverrait
  `session.message`, avec le texte des messages).
- Toutes les 3 s : `sessions.list` avec `sortBy: "activity"`,
  `activeMinutes` (24 h par défaut), `excludeCron`, `excludeSystem`,
  `excludeSubagents`, `requireLastInteraction`, 24 lignes au plus, **sans**
  `includeDerivedTitles`, `includeLastMessage` ni `includeActivitySummary`.
- Champs décodés : `key`, `agentId`, `label`, `displayName`, `updatedAt`,
  `lastActivityAt`, `hasActiveRun`, `status`, `totalTokens`,
  `totalTokensFresh`, `contextTokens`. Jamais `derivedTitle`,
  `lastMessagePreview`, `activitySummary`, `lastRunError`, `subject`,
  `agentStatus` ni `observerDigest`.
- `displayName` peut être un titre généré par OpenClaw à partir du premier
  message, comme `ai-title` chez Claude ; `label` (nom saisi dans OpenClaw)
  passe avant.
- Les charges utiles d'événements ne sont jamais décodées, sauf le défi de
  connexion. Les erreurs ne gardent que des codes (`error.details.code`,
  `requestId`) ; `error.message` n'est pas lu. Ne jamais journaliser de trame
  brute ni un token.

## Correspondance vers AgentsMon

| AgentsMon | OpenClaw |
| --- | --- |
| identifiant | `key` de session (stable, contrairement à `sessionId`) |
| dossier / groupe | `openclaw/<agentId>` : une carte par agent |
| titre | `label`, sinon `displayName` |
| actif | `hasActiveRun` ou `status` `running`/`queued` |
| contexte | `totalTokens / contextTokens` si `totalTokensFresh` n'est pas faux |
| clic | Control UI : `<url>/chat/<agentId>/…`, portage de `buildControlUiSessionPath` (`packages/session-url-contract`) sans le slug dérivé du titre |

## Reprise et échecs

- Réseau ou protocole : nouvelle tentative avec délai croissant (3 s à 60 s).
- Identifiants refusés, appairage requis, refus de méthode : le worker **attend
  une action de l'utilisateur**. La Gateway limite les échecs d'authentification
  par IP, et une boucle bloquerait aussi le navigateur de l'utilisateur.
- Seul `AUTH_DEVICE_TOKEN_MISMATCH` efface le jeton d'appareil stocké ; un
  `DEVICE_AUTH_*` signale un défaut de la preuve (bug ou horloge) et le garde.
- Le fil réseau ne tient jamais le verrou partagé pendant une entrée/sortie ;
  le thread GPUI lit seulement l'état en cache.
