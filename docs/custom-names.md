# Noms personnalisés persistants

Depuis le correctif du 17 septembre 2026, les noms saisis dans la fenêtre
« Renommer » d’AgentsMon sont des préférences persistantes. Auparavant, ils
étaient uniquement en mémoire et étaient aussi supprimés lorsque la session
disparaissait du snapshot d’un provider.

## Comportement

- Un nom de session est associé au couple provider / identifiant de session.
  Il revient quand la même session est reprise, même après un redémarrage.
  Une nouvelle conversation portant un autre identifiant conserve son propre nom.
- Un nom de groupe est associé à sa clé de regroupement, actuellement le chemin
  du répertoire de travail. Il s’applique quand un groupe se reforme dans ce
  répertoire, même avec de nouvelles conversations. Un chemin différent désigne
  un groupe différent ; aucun rapprochement automatique de chemins n’est fait.
- Un groupe réduit à une seule session devient une carte de session, conformément
  au rendu existant. Son nom de groupe reste enregistré et revient dès que le
  groupe contient de nouveau plusieurs membres.
- Les préférences survivent aux snapshots vides, aux sessions masquées et aux
  périodes où aucun agent ne tourne. Elles n’expirent pas automatiquement.
- Valider un nom vide supprime la personnalisation enregistrée et rétablit le
  nom de projet utilisé par défaut. Annuler la fenêtre ne change rien.
- Le renommage est immédiatement diffusé à tous les panneaux et à l’IPC local,
  comme auparavant. Les noms restent gérés par AgentsMon ; ils ne renomment pas
  la conversation dans Claude ou Codex.

## Stockage

Sur macOS :
`~/Library/Application Support/com.oliv.AgentsMon/custom-names.json`.
Sur les autres plateformes, utiliser le `data_dir` de
`directories::ProjectDirs::from("com", "oliv", "AgentsMon")`.

Exemple synthétique :

```json
{
  "version": 1,
  "sessions": [
    {
      "id": { "provider": "claude", "session_id": "session-id" },
      "name": "Tests API"
    }
  ],
  "groups": [
    {
      "key": { "Directory": "/chemin/du/projet" },
      "name": "Projet local"
    }
  ]
}
```

Seuls les noms saisis dans AgentsMon et leurs clés sont enregistrés. Aucun
snapshot, état d’activité, titre fourni par un provider, message, PID ou cible
de focus n’entre dans ce fichier. Les sessions masquées conservent leur fichier
distinct `hidden-sessions.json`. L’ordre des cartes reste en mémoire.

`AgentStore` expose les préférences triées via `custom_names()` et les restaure
avant le premier snapshot via `restore_custom_names()`. Il ne fait aucune E/S.
`agentsmon/src/custom_names.rs` porte la lecture et l’écriture au niveau de
l’application ; le rendu et les providers ne changent pas.

## Durabilité et erreurs

La sauvegarde a lieu pendant le broadcast consécutif au renommage, avant le
retour au traitement des événements. Elle ne dépend pas d’un callback de
fermeture : tuer le processus après validation ne doit pas perdre le nom.
Les broadcasts suivants n’écrivent rien quand les préférences sont inchangées.

Chaque écriture utilise un fichier temporaire unique, puis `sync_all` et un
remplacement atomique. Sur Unix, le fichier est créé avec le mode `0600` et le
répertoire est synchronisé si le système de fichiers le permet. Sur Windows,
le handle temporaire est fermé avant `MoveFileExW` avec remplacement et
`WRITE_THROUGH`. Une écriture échouée ne remplace pas le dernier fichier valide.

Une erreur de sauvegarde laisse les noms en mémoire et écrit un avertissement
explicite dans les logs. Le même contenu est réessayé après au moins trente
secondes lors d’un broadcast ; une nouvelle modification peut réessayer
immédiatement. Cela évite les tentatives et logs à chaque rafraîchissement.
Une erreur persistante du disque peut empêcher la survie au redémarrage ; elle
n’empêche pas AgentsMon de fonctionner.

La lecture et l’écriture sont limitées à 4 Mio. Un fichier absent signifie
aucune personnalisation. Un JSON invalide, une version inconnue ou une erreur
de lecture est journalisé sans recopier ses valeurs. Le fichier original reste
intact pendant le démarrage et les rafraîchissements. Au premier nouveau
renommage, il est déplacé vers `custom-names.json.recovery.<horodatage>` avant
d’écrire les nouvelles préférences. Si ce déplacement échoue, l’écriture est
abandonnée et sera réessayée ; l’original n’est pas écrasé.

Pour examiner une personnalisation manquante, vérifier le fichier, l’identifiant
de session complet et la clé de groupe. Pour récupérer une sauvegarde, arrêter
AgentsMon, inspecter le fichier `.recovery.*`, restaurer uniquement les noms
souhaités dans le schéma v1, puis relancer. Les anciens noms déjà perdus avant
l’installation de ce correctif n’existaient pas sur disque et ne peuvent pas
être restaurés depuis ce fichier.

## Vérifications

Tests automatisés :

```sh
cargo test -p agentsmon-core
cargo test -p agentsmon custom_names
cargo test --workspace
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

Ils vérifient la restauration après un snapshot vide, l’isolation entre
providers, la reformation des groupes, l’effacement des noms, un redémarrage
simulé immédiatement après sauvegarde, les caractères Unicode/guillemets,
les fichiers invalides ou futurs, leur conservation, la limite de taille et
la reprise après un échec d’écriture.

Contrôle manuel : renommer une carte et un groupe, redémarrer AgentsMon, vérifier
les libellés sur les panneaux et dans les résultats de LeSwitcheur ; fermer puis
reprendre une session ; enfin vider les deux noms, redémarrer et vérifier que
les personnalisations ne réapparaissent pas.
