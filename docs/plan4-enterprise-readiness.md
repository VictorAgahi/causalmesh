# Plan 4 (P3) — Préparation d'un pilote entreprise

> **Statut** : plan d'exécution, prêt à être confié à un agent. Point de départ : `main` en **6.0.1**,
> CI verte. Chaque jalon donne un constat vérifié dans le code, le travail, les fichiers touchés et un
> critère de sortie **chiffré**. Les références `fichier:ligne` datent de la 6.0.1 : l'agent les
> revérifie avant d'agir. Aucun chiffre sans source : une mesure manquante est mesurée, ou déclarée
> manquante.
>
> Historique : une première version (non versionnée) partait de prémisses fausses (graphe « à moitié
> muté » alors que le snapshot est atomique, masquage des JWT par entropie qui n'existe pas, sockets
> dans `/tmp`, `stats` à créer alors qu'il existe). Elle a été réécrite contre le code (PR #32), puis
> ses points ouverts ont été tranchés (PR #33). Ce document intègre ces décisions dans le corps des
> jalons : il n'y a plus de section de décisions séparée à réconcilier.

---

## 1. Règles communes à tous les jalons

**Processus par jalon** : une branche partant de `main`, une PR vers `main`, un constat mesuré
avant le correctif, des chiffres avant/après sur le même corpus, une review adversariale dont les
findings (MINOR et au-delà) sont corrigés avant le merge. Personne ne merge sans l'accord de
l'utilisateur.

**Fichiers partagés, propriété exclusive de l'orchestrateur** (sinon conflits garantis) :
- `CHANGELOG.md` : chaque PR ajoute à la place un fragment `changelog.d/4.x-<slug>.md` (sections
  Keep a Changelog : `### Added` / `### Changed` / `### Fixed`). L'orchestrateur consolide à la
  clôture.
- Version (`Cargo.toml` `[workspace.package] version`, `.agents/mesh-mcp.toml`) : aucune PR de jalon
  n'y touche. Clôture du Plan 4 : **6.1.0**.
- `docs/quality.md` : une PR n'y **ajoute** qu'une section datée à la fin (avant « What's NOT
  measured yet »), jamais de réécriture d'une section existante.
- `Cargo.lock` : une PR qui ajoute une dépendance le régénère après chaque rebase
  (`cargo update -w`), jamais de résolution de conflit à la main.

**Environnement** (constats des sessions précédentes) :
- Builds et tests en `--release` : en debug, `rustc` se bloque plusieurs minutes sur le `dlopen` des
  proc-macros sur la machine de dev. Tests ciblés après chaque modification ; la CI fait la
  validation complète.
- Benchs et tests d'intégration avec un `HOME` isolé et un `MESH_SOCKET_PATH` court : un `meshd`
  personnel tourne sur le socket par défaut et ne doit jamais être touché.
- Corpus de bench **hors de tout chemin ignoré par git** : sous `target/`, le watcher ignore les
  modifications et `reload_ms` vaut `None`.
- `scripts/golden/score.py` appelle le `mesh-mcp` du `PATH` : mettre le binaire compilé en tête du
  `PATH`.
- Aucun parcours du disque hors du dépôt, de ses worktrees, de `target/scale-bench` et de
  `~/.cache/mesh-golden`.

---

## 2. Vue d'ensemble

| Jalon | Sujet | Exécution | Critère de sortie |
|---|---|---|---|
| **4.0** | Filtre CI pour les PR empilées | Agent | CI déclenchée sur une PR ciblant une branche `p3/*` |
| **4.1** | Diagnostics d'indexation visibles par l'agent | Agent | Note de rejet dans le résultat, sortie ≤ 48 KB |
| **4.2** | Watcher macOS sans polling, rechargement différé pendant Git | Agent | CPU au repos < 0,5 % ; 0 snapshot pendant un checkout |
| **4.3** | Budgets par taille (5k / 50k / 200k) | Agent + humain (tailles réelles, facultatif) | Bench vert aux 3 paliers |
| **4.4** | Cache SQLite par workspace, quota, éviction | Agent | Taille ≤ quota ; 0 `SQLITE_BUSY` à 4 processus |
| **4.5** | Recall gRPC TypeScript + ratchet golden en CI | Agent | `otel-demo` recall ≥ 90 %, précision ≥ 87,5 % |
| **4.6a** | Matrice d'impact | Agent | Test d'intégration, latence < 100 ms |
| **4.6b** | Ruptures wire-format (`git show`) | Agent | 3 règles testées + cas limites |
| **4.7** | `doctor --fix`, droits du socket, version | Agent | Test d'intégration de réparation |
| **4.8** | Installation pilote + `stats` | Agent (script, `stats`) + humain (pilote) | Script idempotent ; `stats` sans estimation |
| **4.9** | Mémoire YAML/Markdown sous budget | Agent | Pic mesuré, borné si > budget |
| **4.10** | Sandbox réseau du démon (Linux) | Agent | `EPERM` sur connexion sortante |
| **4.11** | Masquage des secrets dans les fixtures | **Humain** (données du pilote) | Décision fondée sur les données |
| **4.12** | Dette : déterminisme CI, audit, fences, homonymes | Agent | Un test de non-régression par point |

---

## 3. Matrice de conflits et vagues

Fichiers de code touchés par jalon (en plus des fragments `changelog.d/`) :

| Jalon | Fichiers |
|---|---|
| 4.0 | `.github/workflows/ci.yml` |
| 4.1 | `crates/mesh-core/src/health.rs`, `state.rs`, `crates/mesh-server/src/indexer.rs`, `tools/smart_search.rs`, `tools/find_dependents.rs`, `cli/doctor.rs` |
| 4.2 | `crates/mesh-core/src/watcher.rs`, `crates/mesh-server/src/watcher.rs`, `scripts/test_git_storm.sh` (nouveau) |
| 4.3 | `scripts/bench/*`, `.github/workflows/nightly-bench.yml`, `scripts/bench/budgets*.json` |
| 4.4 | `crates/mesh-core/src/index_cache.rs`, `config.rs`, `crates/mesh-server/src/indexer.rs` (appel du quota), `crates/mesh-server/src/main.rs`, `crates/mesh-daemon/src/main.rs` (ouverture par workspace) |
| 4.5 | `crates/mesh-parsers/src/languages/typescript.rs`, `crates/mesh-core/src/contracts.rs` (filtre health), `.github/workflows/golden.yml` (nouveau) |
| 4.6a | `crates/mesh-server/src/tools/analyze_impact.rs`, `crates/mesh-parsers/src/markdown.rs` (formateur d'impact) |
| 4.6b | `crates/mesh-server/src/tools/analyze_grpc.rs`, `crates/mesh-parsers/src/languages/proto.rs` |
| 4.7 | `crates/mesh-server/src/cli/doctor.rs`, `crates/mesh-core/src/socket.rs`, `crates/mesh-daemon/src/main.rs`, `crates/mesh-server/src/main.rs` |
| 4.8 | `scripts/install_pilot.sh` (nouveau), `crates/mesh-server/src/cli/stats.rs` |
| 4.9 | `crates/mesh-core/src/properties.rs`, `docs.rs`, `crates/mesh-parsers/src/languages/spec_shape.rs` |
| 4.10 | `crates/mesh-daemon/src/sandbox.rs` (nouveau), `crates/mesh-daemon/src/main.rs`, `Cargo.toml` (dépendance) |
| 4.12 | `scripts/determinism.sh`, `crates/mesh-server/src/tools/mod.rs`, `crates/mesh-parsers/src/markdown.rs`, `crates/mesh-server/src/indexer.rs` (`repo_names`), `crates/mesh-server/src/lib.rs` |

Recouvrements : `indexer.rs` (4.1, 4.4, 4.12) ; `mesh-daemon/src/main.rs` (4.4, 4.7, 4.10) ;
`mesh-server/src/main.rs` (4.4, 4.7) ; `cli/doctor.rs` (4.1, 4.7) ; `markdown.rs` (4.6a, 4.12).
Deux jalons qui se recouvrent ne tournent jamais en même temps.

**Vagues** (au plus 3 jalons simultanés) :

| Vague | Jalons en parallèle | Prérequis |
|---|---|---|
| 0 | 4.0 seul | — |
| 1 | 4.5 ∥ 4.4 ∥ 4.12a (déterminisme CI) | 4.0 mergé |
| 2 | 4.1 ∥ 4.6b ∥ 4.9 | 4.4 mergé (pour 4.1) |
| 3 | 4.2 ∥ 4.6a ∥ 4.3 | 4.5 mergé (pour 4.6a) |
| 4 | 4.7 ∥ 4.12b–e | 4.1 et 4.4 mergés |
| 5 | 4.10 ∥ 4.8 (partie agent) | 4.7 mergé |
| Clôture | consolidation, version 6.1.0, suites de régression | tout mergé |

Après chaque merge, les branches encore ouvertes intègrent `main` (rebase si la branche n'a pas
encore de PR ouverte, sinon merge de `main` dans la branche, jamais de force-push).

---

## 4. Spécifications

### 4.0 — Filtre CI pour les PR empilées

**Constat.** `.github/workflows/ci.yml` se déclenche sur `push` et `pull_request` vers `main`
seulement (`ci.yml:3-7`). Les PR du Plan 3, qui ciblaient chacune la branche précédente, n'ont eu de
CI qu'une fois mergées. La matrice existe déjà : tests sur ubuntu/macos/windows, déterminisme et
release sur ubuntu/macos.

**Travail.** `pull_request: branches: ["main", "p*/**"]`. `push` reste limité à `main`. La matrice
ne change pas.

**Sortie.** Une PR de test ciblant une branche `p3/…` déclenche la CI (lien du run dans la PR).

---

### 4.1 — Diagnostics d'indexation visibles par l'agent

**Constat.** `IndexHealth` compte les rejets (taille > 384 KB, garde lexicale, binaire, échec de
parse : `crates/mesh-core/src/health.rs`, stocké dans le snapshot `state.rs:34`), mais aucun outil ne
les remonte. Un `smart_search` qui ne trouve rien parce que le fichier dépasse 384 KB ressemble à un
bug.

**Travail.**
- Conserver dans `IndexHealth`, en plus des compteurs, la liste des chemins rejetés avec leur raison
  et leur taille. Bornée à 10 000 entrées par snapshot, en ordre déterministe (tri par chemin).
- `smart_search` et `find_dependents` : si des fichiers rejetés se trouvent **dans le scope de la
  requête** (jamais hors scope), ajouter une note en fin de résultat. Au plus 20 fichiers
  (chemin, raison, taille), puis « et N autres », avec l'action à suivre (« lire le fichier
  directement »).
- Budget : la note est **réservée avant** la pagination des résultats, qui se partagent alors
  `48 KB − taille de la note`. La note est ajoutée en dernier et toujours intacte. Taille max de la
  note : 2 KB.
- `mesh-mcp doctor` affiche le même résumé (compteurs par raison, 10 premiers chemins).

**Sortie.**
- Test d'intégration : un fichier de 450 KB dans le scope → `smart_search` renvoie les autres
  résultats **et** la note sur ce fichier ; sortie ≤ 48 KB même avec une page de résultats pleine.
- Test : un fichier rejeté hors du scope n'apparaît pas.

---

### 4.2 — Watcher macOS sans polling, et rechargement différé pendant Git

**Constat.**
- Sur macOS, `MAX_WATCHED_DIRS = 200` (`crates/mesh-core/src/watcher.rs:69`). Au-delà, bascule sur
  `PollWatcher` (`watcher.rs:192`), qui re-parcourt l'arbre en continu. Cause documentée
  (`watcher.rs:60`) : `FsEventWatcher` redémarre son flux à chaque `.watch()`.
- Un rechargement lancé pendant un `git checkout` lit un disque en cours de modification. Le
  snapshot est cohérent en mémoire (publication atomique) mais mélange les deux branches, et il est
  installé.

**Travail.**
- **macOS** : une surveillance FSEvents **récursive par racine configurée**, le filtre
  `.gitignore`/`exclude_patterns` de la 3.3 étant appliqué en mémoire à chaque événement.
  `PollWatcher` seulement si FSEvents refuse de démarrer (volume réseau), avec un log `warn`
  explicite. Linux et Windows ne changent pas.
- **Git** : pour chaque racine, résoudre le vrai répertoire Git. Si `.git` est un fichier
  (sous-module ou `git worktree`), suivre son pointeur `gitdir:`. Surveiller `HEAD`, `index.lock`,
  `rebase-merge/`, `rebase-apply/` dans ce répertoire. Tant qu'une opération est en cours, accumuler
  les chemins sans recharger. À la fin (verrou disparu et `HEAD` stable pendant la fenêtre de
  debounce), faire **un seul** rechargement groupé.
- **Verrou orphelin** : `index.lock` plus vieux que 30 s **et** aucun processus `git` de
  l'utilisateur (table des processus, sans `lsof`) → log `warn` et reprise normale.
- **Pendant l'attente**, les outils répondent avec le dernier snapshot complet et ajoutent une note
  dans le **texte** du résultat (« opération Git en cours ; index de la génération N »). Pas de champ
  JSON-RPC ajouté, pas d'attente imposée aux appels d'outils.

**Sortie.**
- CPU de `meshd` au repos, sur un dépôt de plus de 200 dossiers, échantillonné toutes les 5 s pendant
  10 min (`ps -o %cpu`) : **moyenne < 0,5 %**, avant/après publié. Aucun `PollWatcher` construit sur
  macOS (log vérifié).
- `scripts/test_git_storm.sh` : checkout de 3 000 fichiers pendant des `smart_search` en boucle →
  aucune génération installée pendant le checkout, une seule après, dont l'empreinte (`mesh-mcp graph
  --format fingerprint`) est égale à celle d'une indexation à froid de la branche d'arrivée.
- Test : sous-module et `git worktree` (`.git` fichier) correctement résolus.

---

### 4.3 — Budgets par taille de dépôt

**Constat.** `scripts/bench/budgets.json` (boot 3 s, RSS 300 MB, recherche p50/p95 300/800 ms) est
calibré et tenu à 5 000 fichiers. À 200 000 fichiers, en 6.0.0 : 15 à 17 s de boot, 630 à 830 MB de
pic d'empreinte, recherche p50/p95 de 400 à 480 / 920 à 1 040 ms (mono-racine).

**Travail.**
- Trois paliers : **5k, 50k, 200k fichiers** (`gen_synthetic.py`, avec et sans `--contracts`). Pour
  50k et 200k : mesurer 3 runs à froid, puis fixer le budget à **1,3 × la médiane mesurée**, dans
  `scripts/bench/budgets-50k.json` et `budgets-200k.json`. Une fois fixés, ces budgets ne sont
  jamais relâchés, seulement resserrés.
- Pic mémoire mesuré avec `/usr/bin/time -l` (« peak memory footprint »), pas avec `ps` (la
  compression mémoire de macOS fausse le RSS au repos).
- Nightly : garder le palier 5k ; ajouter 50k (le palier 200k reste manuel, trop long pour un
  runner).
- **Partie humaine, facultative** : si l'utilisateur fournit les tailles de 2 ou 3 dépôts réels de
  l'entreprise, ajouter un palier correspondant. Sans réponse, les trois paliers par défaut
  suffisent : ne pas bloquer.

**Sortie.** Trois fichiers de budgets ; bench vert à chaque palier ; mesures dans `docs/quality.md`.

---

### 4.4 — Cache SQLite : un fichier par workspace, quota, éviction

**Constat.** `~/.cache/mesh-mcp/index-cache.db` est un fichier unique pour tous les workspaces, sans
éviction (`crates/mesh-core/src/index_cache.rs`). `busy_timeout` 5 s, WAL et `synchronous=NORMAL`
sont déjà en place (`index_cache.rs:100-110`). Aucune mesure de croissance n'existe.

**Travail.**
- **Mesurer d'abord** : taille par entrée ; croissance sur une semaine simulée (script : 30
  changements de branche et 10 rebases sur un corpus de 5k fichiers). Publier les chiffres.
- **Un fichier par workspace** : `~/.cache/mesh-mcp/workspaces/<workspace_id>/index-cache.db`, même
  `workspace_id` que le socket (`mesh_core::workspace_id`).
- Colonne `last_accessed_at` (mise à jour par lot, pas à chaque lecture) ;
  `PRAGMA auto_vacuum = INCREMENTAL` sur les nouvelles bases ; changer la version du schéma.
- **Quota** : `[cache] max_size_mb`, **2048 par défaut**. Contrôle **à l'ouverture et après chaque
  rechargement ayant écrit plus de 100 entrées**. Au-delà du quota, supprimer par lots de 1 000 les
  entrées les moins récemment utilisées jusqu'à 80 % du quota, puis `PRAGMA incremental_vacuum`.
- L'ancien fichier global n'est plus lu ; `doctor --fix` (4.7) le supprime.

**Sortie.**
- Test : insertions au-delà d'un quota de test (10 MB) → taille ramenée ≤ 80 % du quota.
- Test : 4 processus indexant 4 workspaces en même temps → 0 `SQLITE_BUSY`.
- Test : un démon qui recharge 30 fois sans redémarrer reste sous le quota.
- Croissance mesurée publiée.

---

### 4.5 — Recall gRPC TypeScript et ratchet golden en CI

**Constat.** `scripts/golden/score.py otel-demo` : **87,5 % de précision / 53,8 % de recall** (7/13),
inchangé en 6.0.1. Les 6 arêtes `frontend → *` sont manquées. Le frontend importe ses clients depuis
un fichier généré par ts-proto (`import { AdServiceClient } from '../../protos/demo'`), puis fait
`new AdServiceClient(ADDR, ChannelCredentials.createInsecure())`
(`src/frontend/gateways/rpc/*.gateway.ts`). L'extracteur TypeScript ne reconnaît que
`ClientGrpc.getService<XServiceClient>(...)` (NestJS). Arête parasite : `checkout → health`.

**Travail.**
- Une `new_expression` `new <X>Client(...)` crée une arête gRPC **si et seulement si** : (1)
  `<X>Client` est importé dans le fichier, et (2) `<X>` ou `<X>Service` correspond à un **service gRPC
  déclaré dans le graphe** (proto indexé). **Aucune heuristique sur le chemin d'import** (`proto`
  attraperait `prototype`, `protocol`…) : c'est la résolution contre les services déclarés qui
  empêche un `new FooClient()` quelconque de devenir une arête.
- Filtrer `grpc.health.v1.Health` comme service d'infrastructure.
- **Ratchet en CI** : nouveau workflow `.github/workflows/golden.yml` (ubuntu, sur PR et push vers
  `main`). Il met `~/.cache/mesh-golden` en cache (`actions/cache`, clé = hash de
  `scripts/golden/repos.txt`), lance `scripts/golden/fetch.sh`, puis `score.py <repo>
  --fail-under-precision P --fail-under-recall R` avec le binaire compilé en tête de `PATH`. Seuils :
  online-boutique 1.0 / 1.0, bank-of-anthos 1.0 / 1.0, otel-demo = valeurs atteintes par ce jalon.

**Sortie.** `otel-demo` : recall ≥ 90 % (≥ 12/13) et précision ≥ 87,5 % ; les deux autres corpus
restent à 100 % / 100 % ; `golden.yml` vert.

---

### 4.6a — Matrice d'impact

**Constat.** `analyze_impact` renvoie une liste. Le graphe connaît déjà la confiance de chaque arête
(exacte, heuristique, ambiguë) et la racine de chaque nœud.

**Travail.**
- Classer chaque élément impacté : `EXTERNE` (appelé depuis un autre service ou une autre racine) ou
  `INTERNE` (même service).
- Chaque ligne porte la confiance de l'arête.
- Sortie : tableau Markdown compact, ≤ 48 KB, en suivant les règles de pagination et de troncature
  de `smart_search`.
- Pas de catégorie « couverture de test » : le graphe ne relie pas les tests au code qu'ils couvrent.

**Sortie.** Test d'intégration Go + Rust + proto : réutiliser `examples/polyglot-shop` s'il couvre ces
trois langages, sinon ajouter une fixture minimale sous `examples/`. Changement d'une méthode proto →
la matrice liste les handlers et les clients attendus, avec la bonne catégorie. Latence < 100 ms sur
la fixture.

---

### 4.6b — Ruptures wire-format

**Travail.** `analyze_grpc` reçoit un argument `base: Option<String>`.
- **Défaut** : merge-base de `HEAD` et de la branche par défaut distante (`origin/HEAD`). À défaut,
  `HEAD` (comparaison du working tree au dernier commit). **Jamais `HEAD~1`**, qui comparerait au
  commit précédent et non à la branche de base.
- Version « avant » lue par `git show <base>:<chemin relatif>`, en mémoire : aucune écriture disque,
  aucune persistance SQLite.
- Trois règles, chacune signalée `WIRE_FORMAT_BREAKING_CHANGE` : numéro de champ réutilisé pour un
  autre champ ; type de champ incompatible ; champ supprimé sans `reserved` (numéro ou nom).
- Cas limites : fichier absent de `base` → nouveau, aucune rupture ; hors dépôt Git, ou `base`
  introuvable → `isError` explicite ; `git` absent du `PATH` → `isError` explicite.

**Sortie.** Un test par règle, plus un test par cas limite, sur un dépôt Git temporaire créé par le
test.

---

### 4.7 — `doctor --fix`, droits du socket, version

**Constat.** Déjà en place : sockets hors de `/tmp` (`crates/mesh-core/src/socket.rs:48-66`),
nettoyage des sockets orphelins (`cleanup_stale_socket`, `socket.rs:97`), watchdogs de la 3.3, droits
`0700`/`0600` sur le cache et l'audit, fusion non destructive de la config IDE (6.0.1). **Manque** :

**Travail.**
- Droits explicites : dossier du socket en `0700`, socket en `0600`.
- Version : le CLI compare la version annoncée par le démon (`serverInfo.version` à `initialize`) à
  la sienne. En cas d'écart, il affiche un avertissement ; `doctor` le signale ; `doctor --fix`
  arrête **uniquement** ce démon (même utilisateur, socket sous le dossier de MeshMCP).
- `doctor` : `PRAGMA quick_check` sur le cache et l'audit ; ancien cache global présent (4.4).
- `doctor --fix` : supprime les sockets orphelins, les caches corrompus ou obsolètes, l'ancien cache
  global. **Jamais l'audit** : on signale sa corruption, on ne l'efface pas (piste de preuve).
- `doctor --json` pour les scripts d'installation.

**Sortie.** Test d'intégration, avec un `HOME` et un socket isolés : socket orphelin + cache corrompu
+ démon d'une autre version → `doctor --fix` les traite ; un second `doctor` est vert.

---

### 4.8 — Installation pilote et `stats`

**Constat.** `mesh-mcp stats` existe (`crates/mesh-server/src/cli/stats.rs`) : appels par outil, taux
d'erreur, scopes consultés, à partir de l'audit local.

**Travail (agent).**
- `scripts/install_pilot.sh`, idempotent : détecte l'OS et l'architecture, installe le binaire dans
  `~/.local/bin`, lance `mesh-mcp init --write-ide-config` (fusion non destructive depuis la 6.0.1)
  **après confirmation** (`--yes` pour l'automatiser), puis `mesh-mcp doctor`.
- `stats` : ajouter les latences p50/p95 par outil, le taux d'`isError`, le taux de hit du cache
  d'index (4.4) et les redémarrages du démon. **Rien d'estimé** : pas de « tokens économisés »
  calculés contre des lectures que l'agent n'a jamais faites.

**Travail (humain, hors périmètre de l'agent).** Pilote, mesure A/B des tokens avec et sans MeshMCP
sur des tâches réelles, scorecard.

**Sortie.** Deux exécutions successives du script donnent le même état ; `stats` testé sur un audit
de fixture.

---

### 4.9 — Mémoire YAML/Markdown sous budget

**Constat.** `serde_yaml` 0.9 met en mémoire les événements d'un document entier. Le Markdown est lu
en entier et chaque section reste en mémoire (`docs/quality.md`, revue de la 3.5). Le seul plafond
est la limite par fichier (384 KB, 1,5 MB pour les schémas).

**Travail.**
- **Mesurer d'abord** : corpus de specs OpenAPI/AsyncAPI proches de 1,5 MB et de Markdown proches de
  384 KB ; pic d'empreinte par fichier (`/usr/bin/time -l`).
- **Seulement si** le pic par fichier dépasse **4 × la taille du fichier**, ou si le total dépasse le
  budget du palier (4.3) : parseur YAML événementiel et découpage Markdown ligne par ligne, avec un
  budget explicite par fichier. Sinon, publier la mesure et clore.

**Sortie.** Pic mesuré publié ; borné si le seuil est dépassé.

---

### 4.10 — Sandbox réseau du démon (Linux)

**Constat.** Aucune dépendance réseau dans le code. Un RSSI voudra tout de même une garantie au
niveau du noyau.

**Travail.**
- Linux uniquement (`#[cfg(target_os = "linux")]`). Après le bind du socket Unix, filtre seccomp qui
  refuse `socket(AF_INET | AF_INET6, …)`, `AF_UNIX` restant autorisé, plus `PR_SET_NO_NEW_PRIVS`.
- `meshd` démarre sous `#[tokio::main]` et doit binder son socket avant de se confiner : le filtre
  est posé avec **`SECCOMP_FILTER_FLAG_TSYNC`**, pour couvrir tous les threads déjà créés (workers
  et pool bloquant de Tokio, rayon, `notify`).
- Choisir la crate (`seccompiler` recommandé : pur Rust, utilisé par Firecracker) et justifier le
  choix dans la PR.
- Documenter les limites : seul `meshd` est confiné ; `mesh-mcp run` (proxy) et `mesh-mcp graph
  --open` (navigateur) ne le sont pas. macOS (`sandbox_init` déprécié) est hors périmètre.

**Sortie.** `tests/security_airgap.rs` (Linux) : une connexion TCP depuis un thread créé **avant** le
confinement et depuis un thread créé **après** échoue avec `EPERM` ; `AF_UNIX` fonctionne toujours.
Vert sur le runner ubuntu.

---

### 4.11 — Masquage des secrets dans les fixtures (humain)

**Constat.** Le masquage se fait par nom de clé (`PropertyRegistry::is_sensitive_key`,
`crates/mesh-core/src/properties.rs`) sur des lignes `clé: valeur`. Il n'existe aucune détection
d'entropie ; `let token = "eyJ…"` n'est pas masqué. Aucun faux positif réel n'a été rapporté.

**Travail.** Aucun code. Pendant le pilote, compter les masquages par type de chemin (audit). Une
exception ne se conçoit que si des faux positifs gênants sont observés : elle sera opt-in et limitée à
des chemins explicites.

---

### 4.12 — Dette

**a. Porte de déterminisme macOS en CI.** Trois runs ont échoué en 0,4 s sans afficher la moindre
empreinte (`e93eae2`, `e54056f`, `3187567`). Hypothèse à vérifier : `scripts/determinism.sh` tourne
sous `set -euo pipefail` et lit l'empreinte via `… | head -n 1`. Si `mesh-mcp` écrit plus d'une
ligne, `head` ferme le tube, le binaire reçoit SIGPIPE, et `pipefail` fait sortir le script en
silence. **Reproduire, corriger, et faire afficher au script la cause de tout échec.** Sortie :
20 runs CI macOS consécutifs verts (`workflow_dispatch` en boucle ou matrice répétée).

**b. Audit des appels refusés.** Les arguments invalides et les refus RSAH sortent de
`ToolRegistry::invoke` avant l'écriture de l'audit (`crates/mesh-server/src/tools/mod.rs:214`,
`:237`, écriture à `:296`). Les enregistrer avec le statut `ERROR`. Sortie : test vérifiant les
deux entrées.

**c. Blocs de code dans `smart_search`.** `MarkdownFormatter::format_search_entry`
(`crates/mesh-parsers/src/markdown.rs:56`) entoure l'extrait de ```` ``` ````. Un extrait contenant
lui-même une ligne ```` ``` ```` ferme le bloc trop tôt. Utiliser une clôture plus longue que la plus
longue suite de backticks de l'extrait. Sortie : test.

**d. Racines homonymes dans `visualize_mesh`.** `WorkspaceIndexer::repo_names`
(`crates/mesh-server/src/indexer.rs:195`) prend le nom du dossier : deux racines `…/a/api` et
`…/b/api` fusionnent en un seul service. Désambiguïser avec le segment parent, de façon
déterministe. Sortie : test.

**e. Outil inconnu pendant l'indexation.** Pendant la première indexation de `meshd`, un nom d'outil
inconnu reçoit l'erreur d'outil « still indexing » au lieu de `-32602`
(`crates/mesh-server/src/lib.rs`, fonction `respond`). Vérifier le nom avant l'état d'indexation.
Sortie : test.

---

## 5. Clôture (orchestrateur)

1. Consolider `changelog.d/` dans `CHANGELOG.md` sous `## [6.1.0] — <date>`, puis supprimer les
   fragments.
2. Version 6.1.0 : `Cargo.toml`, `Cargo.lock`, `.agents/mesh-mcp.toml`.
3. En release, sur l'arbre final : `scripts/determinism.sh` ; golden des 3 corpus ; bench nightly 5k
   hors dépôt ; paliers 50k et 200k (4.3).
4. Section « Plan 4 closeout » dans `docs/quality.md`, avec les mesures finales et la liste de ce qui
   reste humain (4.8 pilote, 4.11, et 4.3 si les tailles réelles n'ont pas été fournies).
5. Une PR ; merge seulement avec l'accord de l'utilisateur.
