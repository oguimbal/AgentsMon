# États Claude et interruption avec Échap

Investigation et correctif du 16 septembre 2026. Ce document complète
`codex-desktop-ipc.md` ; les deux providers partagent le journal de diagnostic,
la fenêtre de rapport et la limite de notifications.

## Problème et résultats de l’investigation

Dans l’extension Claude de VS Code, soumettre un prompt puis interrompre la
réponse avec Échap pouvait laisser AgentsMon vert indéfiniment. Le processus
Claude restait vivant, ce qui est normal pour une conversation ouverte.

Deux causes ont été identifiées :

1. Le hook `Stop` ne s’exécute pas pour une interruption utilisateur. Ce
   comportement est documenté par Anthropic, ce n’est pas une perte de watcher.
2. `PostToolUseFailure` peut fournir `is_interrupt: true`, mais le helper
   classait tous les échecs d’outil comme une activité en cours. Ce hook seul ne
   couvre pas une interruption pendant la réflexion, avant tout outil.

Sources officielles consultées :

- [Stop](https://code.claude.com/docs/en/hooks#stop)
- [PostToolUseFailure](https://code.claude.com/docs/en/hooks#posttoolusefailure)

La dernière trace de la session concernée était `UserPromptSubmit`, état
`active`. Le fichier compatible Claude Status était aussi actif ; lui donner
priorité n’aurait donc pas corrigé le problème. Aucun texte de conversation
n’a été inspecté pour établir ce constat.

L’examen du code installé a révélé un registre distinct :
`~/.claude/sessions/<pid>.json`. Il décrit le processus et, sur les versions
récentes observées, son état courant. Le registre est lu par l’extension
VS Code elle-même pour ses sessions ouvertes dans d’autres hôtes.

| Version du processus observée | Résultat |
| --- | --- |
| 2.1.266 | Fichier présent, sans `status` ni `statusUpdatedAt` dans les deux processus encore ouverts examinés |
| 2.1.272 | Session ouverte avec `status: idle` et horodatage ; code installé contenant la publication du statut |
| 2.1.273 | Registre et publication du statut ; test réel isolé `busy → idle` après interruption |

La version de l’extension installée n’est pas nécessairement celle d’une
conversation déjà ouverte. Le processus concerné utilisait encore 2.1.266
alors que 2.1.273 était installé. Une fermeture/réouverture de la session Claude
est nécessaire pour charger le nouveau binaire. AgentsMon ne redémarre pas
Claude à la place de l’utilisateur.

Ne pas transformer cette observation en une version minimale garantie : la
première version publiant ces champs n’a pas été établie. La détection porte
sur les capacités du fichier réel, pas sur une comparaison de versions.

## Contrat lu

Exemple synthétique, simplifié :

```json
{
  "pid": 4242,
  "sessionId": "session-id",
  "startedAt": 1789590000000,
  "kind": "interactive",
  "version": "2.1.273",
  "status": "idle",
  "statusUpdatedAt": 1789590100000
}
```

Les dates sont des millisecondes Unix. Seuls ces champs sont désérialisés.
`name`, `detail`, `needs`, `logPath`, les chemins de sockets et les autres champs
inconnus sont ignorés. Le lecteur ne parcourt pas de conversations et ne se
connecte pas au socket de messagerie entre agents.

| `status` | État AgentsMon |
| --- | --- |
| `busy` | Active, vert ; conserve Compacting si le hook courant l’indique |
| `shell` | Idle, gris : tour terminé, shell lancé en arrière-plan encore vivant |
| `waiting` | Waiting, orange |
| `idle` | Idle, gris |
| valeur inconnue | Repli sur les hooks, diagnostic après persistance |

`shell` est publié quand le tour est `idle` mais qu’au moins une tâche
`local_bash` d’arrière-plan n’est pas terminée (serveur de dev lancé par
l’agent, par exemple) : binaire 2.1.285, `status = idle && tâche bash vivante ?
"shell" : status`. Si le modèle attendait ce shell, son tour serait `busy`.
Jusqu’au 30 septembre 2026 AgentsMon l’affichait en vert, ce qui laissait une
session terminée « active » tant que le serveur tournait.

`updatedAt` n’est **pas** utilisé : un renommage ou une autre modification de
métadonnées peut l’avancer sans transition de statut. Le lecteur n’interprète
pas non plus les champs d’activité textuels ou les valeurs estimées par un
modèle.

Le registre est une interface interne, sans contrat public de stabilité ni
numéro de version de schéma. `peerProtocol` décrit une autre interface et ne
permet pas de versionner ce fichier.

## Ordre des événements et protections

Le provider découvre toujours les sessions à partir des `.cstatus` et valide
le processus Claude. Le registre complète uniquement les sessions reconnues.
Pour chaque session vivante :

1. Lire uniquement `sessions/<pid>.json`, dans le répertoire frère de la racine
   `projects` du provider.
2. Vérifier le PID et l’identifiant de session. Une session reprise ailleurs ou
   un ancien fichier pour un PID réutilisé ne doit pas modifier la carte.
3. Vérifier `startedAt` contre l’heure de démarrage du processus obtenue par
   `sysinfo`. Les fichiers d’une génération de processus précédente sont ignorés.
4. Accepter `kind: interactive` ou son absence ; ignorer les catégories connues
   `bg`, `daemon`, `daemon-worker`. Une nouvelle catégorie déclenche un diagnostic.
5. Vérifier que `statusUpdatedAt` n’est ni antérieur à `startedAt` ni supérieur
   de plus de deux secondes à l’heure actuelle.
6. Appliquer le statut seulement si `statusUpdatedAt` est strictement plus récent
   que le dernier hook. Un prompt envoyé après un ancien `idle` redevient donc
   immédiatement actif.

Le helper enregistre désormais `event_timestamp` séparément de `timestamp`.
Une actualisation de status line peut avancer `timestamp` sans changer
`event_timestamp`, l’état ou le dernier événement. À la première actualisation
d’un ancien record, son ancien `timestamp` sert de point de départ. Un vrai
hook avance les deux dates. Son activité vide efface aussi l’ancien nom d’outil.

`PostToolUseFailure.is_interrupt: true` ramène directement à Idle si le hook ne
porte pas d’`agent_id`. L’interruption d’un sous-agent ne prouve pas à elle seule
la fin du tour principal. Un échec ordinaire ou un hook d’une ancienne version
sans ce champ reste Active.

Un hook portant un `agent_id` (`SubagentStart`, `SubagentStop`, outils d’un
sous-agent…) qui calculerait Active sur un record déjà Idle est fusionné comme
un heartbeat : état, activité, dernier événement et `event_timestamp` sont
conservés, et un statut plus récent du registre continue de s’appliquer. Les
demandes d’autorisation d’un sous-agent restent Waiting, et un sous-agent
pendant un tour actif ne change rien.

Constaté le 30 septembre 2026 sur une session CLI 2.1.285 restée verte : registre
`idle`, puis `SubagentStop` environ trois minutes plus tard, sans transcript
correspondant dans `subagents/`. Le binaire contient des agents internes forkés
sans transcript (`away_summary`, `extract_memories`, `auto_dream`…) ; ce sont des
sources possibles, l’émetteur exact n’a pas été établi. L’état AgentsMon juste
avant ce hook, écrasé depuis, n’a pas pu être vérifié.

Ce même cas s’est reproduit le 30 septembre 2026 (session « devops », CLI
2.1.285, hôte Warp) malgré la fusion en heartbeat : le record n’était pas Idle
quand `SubagentStop` est arrivé, car le hook `Stop` avait écrit `active`
(« background task ») à cause de shells en arrière-plan. Deux corrections :

- `Stop` ne reste Active que si `background_tasks` contient du travail
  d’agent (`subagent`, `workflow`, `teammate`, `cloud session`). Des `shell`,
  `monitor`, `MCP task`, `dream` ou `auto-mode scan` seuls donnent Idle. Seul
  le champ `type` de chaque tâche est décodé ; `command` et `description`
  contiennent du texte et ne sont pas lus.
- Côté provider, quand le dernier hook est `SubagentStart`/`SubagentStop`, un
  registre `idle`/`shell` s’applique même s’il est plus ancien que ce hook. Un
  tour principal repris republie `busy`, qui reste soumis à la règle normale.

Même mécanisme le 1er octobre 2026 sur une session VS Code 2.1.281 (« edf ») :
registre `waiting` (question posée), puis `SubagentStop` quatre minutes plus
tard, et la carte repassait verte. La protection couvre donc aussi l’attente :

- côté provider, un registre `waiting` s’applique lui aussi sur un hook
  `SubagentStart`/`SubagentStop` plus récent ;
- côté helper, le record retient si son dernier hook venait d’un sous-agent
  (`subagent`). Un hook de sous-agent Active est fusionné en heartbeat sur un
  record Idle, ou Waiting **du tour principal**. Une demande d’autorisation
  émise par un sous-agent se termine toujours quand ce sous-agent reprend.

Hook `Notification` : seuls `permission_prompt`, `worker_permission_prompt`,
`elicitation_dialog`, `elicitation_url_dialog` et `agent_needs_input` (ou une
notification sans type, anciennes versions) donnent Waiting. `idle_prompt`,
envoyé une minute après un tour terminé resté sans réponse, donne Idle ; les
records écrits avant le 1er octobre 2026 avec ce type sont aussi lus Idle.
`auth_success` reste Active. Les autres types (`agent_completed`,
`push_notification`, `quota_auto_resume_*`, `computer_use_*`,
`model_refusal_fallback`, futurs types) ne changent pas l’état : ils sont
fusionnés comme un heartbeat. Liste relevée dans le binaire 2.1.285.

Limite connue : si un tel agent demande une autorisation après la fin du tour,
la carte passe Waiting puis son `PostToolUse` suivant la remet Active.

Les données restent dans le provider Claude et le helper. Le store, le rendu
GPUI, les autres panneaux et l’export local reçoivent le même `AgentSnapshot`
que pour les autres sources. Aucun comportement Claude n’est ajouté à l’UI.

## Résilience et limites

- Maximum 256 Kio par fichier, y compris si celui-ci grossit pendant la lecture.
- Fichiers réguliers uniquement ; liens symboliques, répertoires et FIFO refusés.
  Unix ajoute `O_NOFOLLOW | O_NONBLOCK` à l’ouverture pour fermer la course avec
  un remplacement par un lien ou un FIFO. Sur Windows, le contrôle initial et
  celui du handle sont appliqués ; cette protection Unix supplémentaire n’existe
  pas dans cette implémentation Windows.
- Les lectures sont de petites lectures locales synchrones, sans commande
  externe, attente réseau ou boucle de polling interne. Comme les `.cstatus`,
  elles dépendent néanmoins de la réactivité du système de fichiers local.
- JSON partiel, erreur d’accès, champ incompatible, horodatage incohérent : aucun
  panic, aucun ancien `idle` conservé en cache, repli sur le dernier hook.
- Un fichier absent n’est pas un incident : anciennes versions, démarrage et
  arrêt normal peuvent tous expliquer cette absence. Une disparition silencieuse
  du registre entier ne peut donc pas être distinguée d’un cas normal.
- Un fichier présent sans statut pour une session non Idle produit un diagnostic
  explicite après le délai ci-dessous. Il peut s’agir d’une ancienne version,
  d’une fonctionnalité désactivée ou d’une évolution du format.
- Le watcher observe également le répertoire `sessions` s’il existe. Le
  rafraîchissement ordinaire de dix secondes couvre sa création ultérieure ou
  l’échec du watcher. Quand le watcher fonctionne, les changements sont regroupés
  avec les autres événements (rafraîchissement au plus une fois par seconde).
- Aucun délai de silence, taux CPU ou temps d’exécution d’un outil ne signifie
  « interrompu ». Un long calcul reste actif.
- Le lecteur conserve un ordre temporel conservateur : un hook plus récent gagne
  sur un registre plus ancien. Un hook tardif émis après la publication d’Idle
  peut donc demander une nouvelle transition du registre pour être corrigé.
- Les horloges locales sont supposées cohérentes ; les valeurs incohérentes sont
  ignorées ou signalées, sans inventer une transition.
- La prise en charge du registre ne rend pas les sessions WSL détectables depuis
  Windows ; cette limite du provider demeure.

## Diagnostics et notifications

Un même problème doit être observé pendant au moins quinze secondes avant
d’être exposé à l’application. Cette temporisation filtre notamment les fichiers
tronqués pendant une écriture en place. Une lecture valide ou l’absence de
problème remet ce délai à zéro. Si plusieurs sessions sont concernées, la
première selon l’ordre stable des identifiants est signalée ; les autres sont
examinées également et conservent leur repli individuel.

| Code | Interprétation |
| --- | --- |
| `claude.registry.schema` | JSON ou champs requis incompatibles/incomplets |
| `claude.registry.read` | Erreur d’accès/lecture ou croissance au-delà de la limite |
| `claude.registry.file` | Type de fichier ou taille initiale refusé |
| `claude.registry.kind` | Catégorie de session inconnue |
| `claude.registry.status` | Valeur de statut inconnue |
| `claude.registry.timestamp` | Ordre ou plage des dates incohérents |
| `claude.registry.status_unavailable` | Fichier sans état complet pour une session non Idle ; relancer Claude avec une version récente |

Les erreurs Serde peuvent contenir une valeur issue de l’entrée : elles ne sont
jamais copiées dans les logs. Les diagnostics contiennent des codes et textes
fixes, le chemin du fichier et le PID. Pour le cas sans statut, la version peut
être ajoutée seulement si elle comporte au maximum 32 caractères, tous chiffres
ou points. Aucun dump du JSON, nom de conversation ou message d’erreur Claude
n’est stocké.

Le mécanisme commun `connection_diagnostics.rs` :

- journalise explicitement l’incident, sa persistance et sa disparition ;
- conserve les rapports et la dernière tentative de notification sur disque ;
- limite les notifications de diagnostic à **une par heure au total**, Claude
  et Codex confondus, y compris après un redémarrage ;
- ouvre un rapport copiable au clic sur la notification ; le menu
  **AgentsMon → Diagnostic des connexions…** ouvre aussi les rapports.

Sur macOS, les fichiers se trouvent dans
`~/Library/Application Support/com.oliv.AgentsMon/diagnostics/` :
`provider-diagnostics.log`, son archive `.1`, et `provider-diagnostics.json`.
Le chemin réellement utilisé est rappelé dans le rapport. Le journal tourne à
512 Kio. Les permissions système peuvent empêcher une notification native ;
les logs et le menu restent utilisables. Voir `codex-desktop-ipc.md` pour les
limites de livraison et de clic des notifications.

## Retrouver le contrat après une mise à jour

1. Relever la version dans le fichier du **processus concerné**, pas uniquement
   dans `claude --version` ou le `package.json` de l’extension.
2. Examiner seulement les champs de métadonnées listés plus haut dans
   `~/.claude/sessions/<pid>.json` et le record AgentsMon correspondant. Ne pas
   chercher `[Request interrupted by user]` dans les messages : cette approche
   enfreindrait la règle de confidentialité et dépendrait d’un texte d’affichage.
3. Dans `~/.vscode/extensions/anthropic.claude-code-<version>-<plateforme>/extension.js`,
   rechercher `liveRegistry`, `session_state_changed`, `sessions`, `busy`,
   `waiting`. Le lecteur installé observé reconnaît quatre états :
   `busy`, `shell`, `idle`, `waiting`.
4. Dans `resources/native-binary/claude`, le JavaScript embarqué observé est
   encore visible. Chercher `statusUpdatedAt`, la publication de
   `{status: ..., waitingFor: ...}`, puis ses appelants. Les noms minifiés changent.
   En 2.1.273, la fonction observée `Bfe` ajoute les dates et écrit le registre ;
   `Hh` relie le lifecycle headless aux valeurs `busy`/`waiting`/`idle`.
5. L’extension envoie une requête de contrôle `{subtype: "interrupt"}` lorsqu’on
   interrompt une session. Le test isolé ci-dessous reproduit cette frontière
   sans toucher à une session utilisateur.
6. Adapter uniquement le lecteur typé et ses tests. Un changement de schéma ne
   justifie pas de désérialiser le fichier entier en `serde_json::Value` ni de
   remplacer le statut explicite par un timeout d’inactivité.

## Vérification reproductible

Tests déterministes :

```sh
cargo test -p agentsmon-providers claude_runtime
cargo test -p agentsmon-helper
cargo test --workspace
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

Les tests couvrent l’interruption sans Stop, le prompt suivant, les quatre
statuts, la compaction, le silence prolongé, les identifiants/PID incorrects,
les générations de processus, les catégories exclues, les anciennes versions,
les dates invalides, JSON tronqué, données trop volumineuses, liens/FIFO et le
délai de diagnostic/récupération. Le helper vérifie aussi le champ interruption,
les sous-agents, l’échec ordinaire et les heartbeats de status line.

Test avec le vrai binaire Claude installé (opt-in) :

```sh
python3 scripts/test-claude-interruption.py --claude /chemin/vers/claude
```

Le script crée une configuration temporaire, utilise `--bare`, une fausse clé
et un serveur HTTP sur `127.0.0.1`. Le serveur maintient une réponse SSE ouverte,
le script attend `busy`, envoie le contrôle d’interruption puis attend `idle`.
Les outils, plugins, MCP et persistance de session sont désactivés. Les variables
d’authentification et de sélection de provider du parent ne sont pas héritées.
Le processus est arrêté dans `finally`, même si le test échoue. Aucun compte
ni API de modèle réelle n’est utilisé ; seules les métadonnées de statut sont
imprimées. Les sorties brutes de Claude sont ignorées.

Exécuté avec succès le 16 septembre 2026 sur le binaire de l’extension VS Code
2.1.273 : `busy → idle` durant une requête modèle synthétique. Cela valide la
frontière d’interruption headless utilisée par VS Code ; ce n’est pas un test
GUI de l’acheminement de la touche Échap.

Contrôle manuel après déploiement : relancer AgentsMon, rouvrir la session
Claude avec le binaire récent, envoyer un prompt, interrompre pendant la
réflexion, puis envoyer un nouveau prompt. Attendre gris après interruption,
vert au nouveau prompt. Refaire pendant un outil et une demande d’autorisation.
Ne pas interrompre une session utilisateur automatiquement pour tester.

Dans une session CLI, terminer un tour, vérifier gris, puis laisser le terminal
sans interaction plus de trois minutes : la carte doit rester grise.
