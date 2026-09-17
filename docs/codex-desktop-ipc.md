# Codex desktop: états d’attente et diagnostic du protocole IPC

Ce document décrit une interface **interne**, constatée sur ChatGPT macOS
**26.903.71938, build 8576**, avec son exécutable Codex **0.153.4**, le
16 septembre 2026. Il ne constitue pas une garantie de compatibilité d’OpenAI.
Ne pas augmenter une version acceptée sans vérifier le contrat et les tests.

## Pourquoi les hooks ne suffisent pas

Le cas initial était une autorisation Computer Use : « Allow ChatGPT to use
LeSwitcheurPreviewLight? ». Codex affichait « Awaiting approval » et AgentsMon
restait vert. Pendant ce blocage, les vérifications indépendantes ont trouvé :

| Source | Observation |
| --- | --- |
| Record AgentsMon | `state=active`, `event=PreToolUse`, `activity=mcp__node_repl__js` |
| Rollout | Dernière écriture à 18:45:23 UTC ; aucune progression pendant l’attente |
| `node_repl/active_execs/*.json` | Exécution présente, mais aucun indicateur d’autorisation |
| `logs_2.sqlite` | Dernier événement technique attribué à la tâche à 18:45:54 UTC ; aucun nouveau hook d’attente |
| SQLite et état global du desktop | Aucun statut d’attente utilisable trouvé pour cette tâche |
| IPC desktop, testé en direct | `threadRuntimeStatus={type:"active",activeFlags:["waitingOnApproval"]}` |
| Métadonnées de demande, sonde d’investigation | `method=mcpServer/elicitation/request` |
| Tâche témoin active | `activeFlags=[]`, aucune demande en attente |

La sonde d’intégration a ensuite retrouvé `idle`, en accord avec le hook `Stop`
de la tâche après sa reprise/fin. Il ne faut donc pas transformer automatiquement
un `node_repl` long en attente utilisateur. Un calcul long produit les mêmes
signaux de silence sur disque.

L’[événement MCP officiel](https://learn.chatgpt.com/docs/app-server#mcp-server-elicitation-requests)
est `mcpServer/elicitation/request`. Son existence explique le comportement, mais
la documentation app-server **ne documente pas** le transport Electron utilisé ici.

### Une piste écartée à tort lors de l’investigation initiale

Les processus app-server du desktop utilisent `stdio://`. Démarrer un autre
app-server ou le daemon ne donne pas accès à l’état en mémoire de ces processus.
En revanche, `~/.codex/ipc/ipc.sock` est un routeur Electron entre clients locaux.
Une sonde JSON-RPC délimitée par des retours à la ligne n’obtient pas de réponse
valide : ce n’est pas son format. Ce socket **permet** de découvrir le propriétaire
d’une tâche et de recevoir son état courant avec le protocole ci-dessous.

## Répartition du code

| Fichier | Responsabilité |
| --- | --- |
| `crates/agentsmon-providers/src/codex_ipc/protocol.rs` | Versions, schémas limités, validation et traduction des états |
| `crates/agentsmon-providers/src/codex_ipc/transport.rs` | Socket Unix, contrôle du propriétaire, frames, limites et délais |
| `crates/agentsmon-providers/src/codex_ipc/mod.rs` | Worker, échantillonnage, découverte du propriétaire, cache et repli |
| `crates/agentsmon-providers/src/codex_ipc/tests.rs` | Faux routeur, panne, confidentialité, cache et sonde réelle facultative |
| `crates/agentsmon-providers/src/codex.rs` | Superposition du statut direct aux hooks/lifecycle |
| `crates/agentsmon/src/connection_diagnostics.rs` | Rapports persistants, journal rotatif, limite globale de notifications |
| `crates/agentsmon/src/notification.rs` | Notification native et capture du rapport pour le clic |
| `crates/agentsmon-ui/src/diagnostic_report.rs` | Fenêtre générique et copie du prompt |

Le core et `MonitorView` ne connaissent pas le protocole Codex. Les changements
du worker passent par `AgentProvider::take_changed()`, puis le même `refresh` /
`broadcast` que les hooks : tous les panneaux et la source IPC AgentsMon reçoivent
le nouvel état. L’IPC **publié par AgentsMon** est un autre contrat : ne pas
modifier `crates/local-source-ipc` pour adapter le protocole privé de Codex.

## Transport et séquence vérifiés

- Endpoint Unix : `$CODEX_HOME/ipc/ipc.sock`, ou `~/.codex/ipc/ipc.sock`.
- AgentsMon ne crée, ne supprime et ne répare jamais ce socket.
- Répertoire privé appartenant à l’utilisateur courant ; socket privé du même
  utilisateur. Liens symboliques et permissions de groupe/autres refusés.
- Chaque message : **4 octets u32 little-endian**, longueur du JSON UTF-8 qui suit,
  puis exactement cette quantité d’octets. Aucun séparateur de ligne.
- `type` vaut notamment `request`, `response`, `broadcast`,
  `client-discovery-request`, `client-discovery-response`.
- Les `requestId` corrèlent les réponses ; `sourceClientId` et `targetClientIds`
  routent les messages. `hostId=local` seulement.

| Message | Version vérifiée |
| --- | ---: |
| `initialize` | 0 |
| `thread-owner-discovery` | 1 |
| `thread-stream-following-changed` | 1 |
| `thread-stream-state-changed` | 11 |

### 1. Initialisation

```json
{"type":"request","requestId":"agentsmon-123-1","sourceClientId":"initializing-client","version":0,"method":"initialize","params":{"clientType":"agentsmon"},"timeoutMs":2000}
```

La réponse doit avoir `resultType=success`, `method=initialize` et
`result.clientId` UUID. Utiliser cet identifiant pour les messages suivants.
Un client inconnu est autorisé ; ne pas se faire passer pour une fenêtre Codex.

### 2. Découverte du propriétaire de la tâche

```json
{"type":"request","requestId":"agentsmon-123-2","sourceClientId":"CLIENT_UUID","version":1,"method":"thread-owner-discovery","params":{"hostId":"local","conversationId":"THREAD_UUID"},"timeoutMs":2000}
```

La réponse réussie fournit `handledByClientId`. `error=no-client-found` est un
cas normal : tâche CLI, fenêtre fermée, tâche déchargée ou sans propriétaire
desktop. Les autres erreurs de contrat donnent un diagnostic.

### 3. Abonnement temporaire et snapshot

```json
{"type":"broadcast","sourceClientId":"CLIENT_UUID","targetClientIds":["OWNER_UUID"],"method":"thread-stream-following-changed","version":1,"params":{"conversationId":"THREAD_UUID","hostId":"local","following":true}}
```

Le propriétaire publie `thread-stream-state-changed`. Exemple **synthétique**
réduit aux champs d’intérêt ; le message réel contient d’autres champs qui ne
doivent jamais être décodés ou enregistrés :

```json
{
  "type": "broadcast",
  "method": "thread-stream-state-changed",
  "sourceClientId": "OWNER_UUID",
  "version": 11,
  "params": {
    "conversationId": "THREAD_UUID",
    "hostId": "local",
    "change": {
      "type": "snapshot",
      "revision": 252,
      "conversationState": {
        "id": "THREAD_UUID",
        "threadRuntimeStatus": {
          "type": "active",
          "activeFlags": ["waitingOnApproval"]
        }
      }
    }
  }
}
```

Vérifier l’émetteur, la version, l’hôte et les deux identifiants de tâche. Ne pas
accepter un statut d’une autre tâche. Un `change.type=patches` est ignoré :
attendre le snapshot complet. Ses valeurs ne sont pas analysées.

### 4. Désabonnement

Envoyer exactement le message d’abonnement avec `following=false`, sur succès
**et sur erreur**. Si l’envoi échoue, fermer la connexion. La déconnexion fait
désinscrire le client au routeur. L’abonnement temporaire n’accepte aucune
autorisation, ne marque pas la tâche lue et ne demande aucun changement de rôle.
Le desktop peut maintenir son état chargé pendant ce bref abonnement.

Le routeur peut demander si AgentsMon sait traiter des commandes via
`client-discovery-request`. Répondre `client-discovery-response` avec
`response.canHandle=false`. Ne jamais proposer un handler de commande ou réclamer
la propriété d’une tâche.

## États, fraîcheur et limites

| Statut direct | Résultat |
| --- | --- |
| `active` + `waitingOnApproval` | `Waiting` / orange |
| `active` + `waitingOnUserInput` | `Waiting` / orange |
| `active` + tableau vide | `Active` / vert, sauf compaction explicite conservée |
| `idle` | `Idle` / gris |
| `notLoaded`, `systemError` | Aucun remplacement ; utiliser les hooks/lifecycle |
| État ou flag inconnu, `activeFlags` absent pour `active` | Diagnostic et repli |

- Worker unique par provider, aucune attente réseau sur le thread GPUI.
- Les sessions éligibles ont un processus valide et un état de repli non idle.
  Les sessions archivées/expirées sont exclues avant cette étape.
- Au plus **32 sessions**, les plus récemment actives en priorité ; les autres
  gardent les hooks. Les constructeurs de provider destinés aux tests de fichiers
  n’ouvrent aucune connexion au vrai desktop.
- Échantillonnage nominal toutes les **3 secondes**, réveil sur changement des
  cibles. Une connexion est réutilisée ; le propriétaire est redécouvert à chaque
  échantillon pour supporter le changement de fenêtre.
- **3 secondes par opération** (connexion, init, découverte, lecture complète,
  abonnement/désabonnement). Le délai est global pour la frame : un émetteur qui
  ne fournit que quelques octets ne peut pas prolonger indéfiniment la lecture.
- Les cibles sont interrogées séquentiellement : sous forte charge, la fréquence
  par cible peut diminuer. La fraîcheur est prioritaire sur la conservation d’un
  statut ancien.
- Cache valable **moins de 15 secondes** ; rejet dès que le timestamp du hook
  change. Un nouveau hook ne peut pas hériter de l’autorisation du tour précédent.
  Les événements explicites de fin détectés par le provider restent prioritaires.
- Les mises à jour sont regroupées et affichées via le refresh habituel (au plus
  une fois par seconde sur signal ; refresh périodique normalement 10 secondes).
  Une expiration silencieuse apparaît donc au refresh périodique suivant.
- Trames limitées à **16 Mio** (l’application autorise davantage). Allocation
  fallible, JSON borné par la frame, récursion Serde limitée, au plus 16 flags et
  128 messages intercalés avant une réponse. Un snapshot plus gros provoque un
  repli documenté, pas une hausse automatique de la limite.
- Après une erreur : cache invalidé, connexion abandonnée. Reconnexion graduelle
  de 6 à 60 secondes ; incompatibilité suspectée : 60 secondes. Les hooks ne
  contournent pas ce délai. Pas de boucle de reconnexion agressive.
- Un panic du worker est intercepté ; le cache est écarté et un diagnostic est
  signalé. Aucun `join` bloquant à l’arrêt de l’interface.
- Support direct Unix ; **Windows reste sur hooks/lifecycle**. Ne pas annoncer
  le support du named pipe Windows sans implémentation et tests correspondants.

L’absence de socket, sa fermeture et l’absence de propriétaire ne sont pas des
preuves de changement de protocole. Elles ne génèrent pas de notification de
compatibilité. Une évolution qui rend toute découverte indistinguable d’un
`no-client-found` ne peut pas être identifiée avec certitude par ce seul contrat.

## Confidentialité

Le wire format du desktop transporte potentiellement toute une conversation.
Le worker reçoit les octets bornés en mémoire le temps du décodage, puis les
libère. Il ne décode que l’enveloppe, les identifiants de routage et
`threadRuntimeStatus`. Les titres, previews, tours, messages, demandes,
arguments/résultats d’outils et patches sont ignorés par Serde.

`RawValue` est uniquement une **référence empruntée** dans la frame temporaire
pour choisir le schéma après vérification de l’enveloppe. Ne jamais en conserver
une copie, l’imprimer, l’écrire sur disque, ni utiliser `serde_json::Value` pour
une réponse réelle. `Value` est réservé aux requêtes construites par AgentsMon
et aux données synthétiques des tests.

Les erreurs Serde peuvent inclure des valeurs d’entrée dans leur texte. C’est
pourquoi seuls leur catégorie, ligne et colonne sont consignés. Les valeurs
inconnues de statut/flag ne sont pas reproduites dans le rapport.

## Notifications et fichiers de diagnostic

Un incident détecté produit :

1. Un événement explicite `agentsmon::codex_ipc` dans tracing, avec code, phase,
   versions, limites et repli.
2. Un événement JSON dans **`diagnostics/provider-diagnostics.log`**, même si
   `RUST_LOG` masque les warnings ou si Finder ne conserve pas stderr.
3. Un rapport dans **`diagnostics/provider-diagnostics.json`**, avec les dates,
   le dernier incident de chaque provider et la réservation de notification.
4. Au plus **une notification par heure**, globalement pour ces diagnostics,
   tous codes/providers confondus. Cette limite est persistée **avant** l’envoi,
   et reste valable après redémarrage, changement d’erreur et rétablissement.

Sur macOS, le répertoire est normalement :

```text
~/Library/Application Support/com.oliv.AgentsMon/diagnostics/
```

Le log tourne à **512 Kio**, avec un seul précédent `.log.1`. Les répétitions
identiques sont consignées au plus une fois par heure ; un nouvel incident ou
un incident qui cesse d’être signalé est consigné immédiatement. La limite de
notifications d’incidents ne s’applique pas aux notifications ordinaires des agents.

Le clic ouvre un rapport avec **« Copier le prompt de diagnostic »**. Le prompt
inclut les versions attendues/observées disponibles, le code, la phase, les
horodatages, le socket, l’ID technique de session, les chemins des logs et ce
document. Il n’est pas envoyé automatiquement. Il est également accessible dans
**AgentsMon → Diagnostic des connexions…**, même après un redémarrage ou si les
notifications natives sont désactivées.

Le callback capture le rapport envoyé : une réparation ultérieure ne remplace
pas les détails de la notification par « tout va bien ». La notification de
diagnostic n’a pas de TTL applicatif court : elle reste cliquable dans le centre
de notifications pendant l’exécution d’AgentsMon. Le système garde le contrôle
de l’affichage des bannières. Après arrêt de l’application, les handlers natifs
en mémoire ne sont plus disponibles ; utiliser le rapport persistant du menu.

Si le dossier de diagnostic n’est pas inscriptible, un warning l’indique. La
limite reste active en mémoire mais sa persistance entre redémarrages ne peut pas
être garantie. Un recul de l’horloge ne déclenche pas de notifications répétées.
Le rapport indique « incident non signalé » lorsqu’un incident disparaît ; cela
ne constitue pas une preuve que le desktop est connecté (il peut être arrêté).

## Procédure lorsque Codex change

### Lecture des codes d’incident

| Code | Première vérification |
| --- | --- |
| `stream_version_changed` | Version de `thread-stream-state-changed` dans la table du client Electron |
| `schema_changed` | Structure de l’enveloppe/du snapshot à la phase indiquée ; seules catégorie et position JSON sont conservées |
| `initialize_rejected`, `initialize_result_missing`, `invalid_client_id` | Réponse d’initialisation et format de `result.clientId` |
| `owner_response_changed`, `invalid_owner_id` | Découverte du propriétaire, version 1 et `handledByClientId` |
| `snapshot_missing`, `snapshot_state_missing`, `session_mismatch` | Routage local et chemin `params.change.conversationState` |
| `unknown_change_type` | Nouvelles variantes de `change.type` ; ne pas interpréter un patch comme un snapshot |
| `missing_active_flags`, `unknown_active_flag`, `unknown_runtime_status` | Enum du statut et des flags ; vérifier leur sémantique avant de les accepter |
| `frame_size` | Framing little-endian et taille observée ; conversation trop volumineuse possible, sans changement du protocole |
| `response_timeout` | Phase concernée, desktop temporairement bloqué ou réponse attendue ayant changé |
| `message_limit` | Routage trop large, flux parasite, ou snapshot attendu qui n’arrive pas |
| `allocation_limit` | Limites de mémoire et taille du snapshot ; ne pas retirer les bornes |
| `worker_start_failed`, `worker_panicked`, `encode_error` | Problème local AgentsMon ; inspecter l’environnement et le worker |
| `transport_unavailable` | Socket absent/fermé/refusé ; repli normal, sans notification de protocole |

### Investigation

1. Copier le prompt depuis la notification ou le menu. Conserver les logs
   techniques ; ne pas copier une conversation, un rollout complet ou une DB.
2. Examiner `code`, `phase`, `expected` / `observed`, la taille des trames et
   l’erreur IO. Distinguer version inconnue, schéma manquant, délai et socket absent.
3. Vérifier la version de l’application (`Contents/Info.plist`) et du binaire
   embarqué (`Contents/Resources/codex --version`). La commande `codex` du PATH
   peut être une autre version, voire une installation npm cassée.
4. Inspecter **le code applicatif**, pas les données des conversations. Dans
   `/Applications/ChatGPT.app/Contents/Resources/app.asar`, les fichiers de la
   version initiale étaient :
   - `.vite/build/src-B6LqG3ek.js` : `IpcClient`, `IpcRouter`, framing et versions ;
   - `webview/assets/app-initial-a9514281e192.js` : `threadRuntimeStatus`,
     `handleThreadStreamFollowingChanged`, `sendConversationSnapshot` ;
   - `.vite/build/main-Bkkz0ENj.js` : raccordements au desktop/app-server.
   Les suffixes changent avec les builds. Rechercher les chaînes stables
   `ipc.sock`, `readUInt32LE`, `thread-owner-discovery`,
   `thread-stream-following-changed`, `thread-stream-state-changed`,
   `waitingOnApproval`, `mcpServer/elicitation/request`.
5. Pour extraire un ASAR sans dépendance npm : lire les quatre u32 little-endian
   à l’offset 0 ; le quatrième est la longueur du JSON d’index à l’offset 16.
   Les données commencent à `8 + deuxième_u32`. Parcourir `files` dans l’index ;
   les offsets de fichier sont relatifs à ce début. Écrire seulement les fichiers
   JavaScript sélectionnés dans un dossier temporaire. Ne pas modifier l’app.
6. Vérifier la séquence ci-dessus, les noms/champs et les nouvelles versions dans
   le code. Réviser les décoders typés et les tests synthétiques avant d’accepter
   une nouvelle version. Un numéro différent ne prouve pas la compatibilité.
7. Utiliser la sonde explicite ci-dessous avec un UUID obtenu dans un record de
   métadonnées AgentsMon. Observer une autorisation réellement affichée et une
   tâche active témoin ; ne jamais accepter/refuser une autorisation pour sonder.
8. Faire passer les tests de panne et confidentialité, puis les contrôles du
   workspace. Mettre à jour ce document avec la nouvelle version vérifiée et les
   différences observées.

## Tests et commandes

```sh
cargo test -p agentsmon-providers codex
cargo test -p agentsmon connection_diagnostics
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
claude plugin validate .
python3 scripts/validate-plugin.py
```

Les tests IPC créent exclusivement des sockets temporaires et des données
synthétiques. Un environnement sandbox peut interdire `bind`/`connect` avec
`EPERM` : autoriser ces tests locaux, ne pas désactiver leur assertion pour les
faire passer. La sonde réelle est ignorée par défaut :

```sh
AGENTSMON_CODEX_DIAGNOSTIC_SESSION=UUID_EXPLICITE \
  cargo test -p agentsmon-providers live_desktop_status -- --ignored --nocapture
```

Elle affiche seulement l’ID fourni et `Some(Waiting)`, `Some(Active)`, `Some(Idle)`
ou `None`. Une panne affiche uniquement un `Fault` nettoyé. Elle se désabonne et
ne conserve ni le snapshot ni les messages.

Les tests couvrent : états et flags, schéma/version inconnus, payload privé
ignoré, source/tâche différentes, patches ignorés, refus d’offrir des commandes,
désabonnement sur erreur, propriétaire/socket absent, fragmentation, taille
excessive, lecture partielle bloquée, EOF, cache périmé/nouveau hook, worker,
limite horaire après redémarrage/rétablissement, journal rotatif et stockage
inaccessible. Les notifications natives et la copie du rapport nécessitent
aussi une vérification de l’interface sur la plateforme ciblée.

### Vérification isolée de la fenêtre

```sh
cargo run -p agentsmon --example diagnostic_preview
```

Cet exécutable ouvre le véritable `DiagnosticReportView` avec un rapport
synthétique marqué **TEST LOCAL**. Il ne démarre aucun provider, n’installe aucun
hook, n’affiche aucune barre et ne touche pas au dossier de diagnostics réel.
Vérifier le défilement, le redimensionnement et « Copier le prompt de diagnostic ».
Le texte complet est copié, même lorsque certaines lignes sont hors de la zone
visible. Ce test a été effectué sur macOS pendant le développement du correctif.

Pour tester aussi l’envoi natif et le clic, ajouter `--notify` à **cet exemple**.
Sur macOS, le binaire doit être placé dans un bundle `.app` avec un
`CFBundleIdentifier` et l’autorisation de notifications. Utiliser une identité de
test distincte pour ne pas remplacer le handler de l’instance AgentsMon ouverte.
L’exemple utilise le même `NativeNotifications` que l’application ; son clic
rouvre le rapport capturé. La notification porte explicitement le nom **Codex —
TEST**. Elle ne consomme pas la limite horaire du dossier de diagnostics réel.

### Validation initiale du correctif

- Les 109 tests automatisés du workspace passent sur macOS (deux tests ignorés
  par défaut : sous-processus fixture Vibe et sonde réelle Codex).
- `cargo check --workspace`, Clippy avec `-D warnings`, `cargo fmt --check`,
  `claude plugin validate .` et `scripts/validate-plugin.py` passent.
- La sonde du code intégré communique avec l’application installée et retrouve
  `Idle` pour la tâche initiale, qui avait entre-temps émis `Stop`.
- L’investigation initiale avait mesuré `waitingOnApproval` sur cette même tâche
  pendant le blocage ; les faux routeurs couvrent cet état et la dérive vers une
  version 12, jusqu’à la publication du diagnostic par le worker.
- La fenêtre et son bouton de copie ont été vérifiés dans le bundle de test.
  L’affichage/clic d’une notification native réelle et Windows restent des
  vérifications manuelles distinctes ; ne pas les déduire d’un test de quota.
