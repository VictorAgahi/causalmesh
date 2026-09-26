# Plan 4 (P3) — Préparation d'un pilote entreprise

> **Statut** : proposition, révisée le 2026-09-26 à partir d'une première version confrontée au
> code, puis complétée par une revue externe dont les points ouverts sont tranchés dans
> « Décisions verrouillées ». Chaque jalon part d'un **constat vérifié dans le code** (état 6.0.0,
> Plan 3 mergé dans `main` avec CI verte sur `1f1ffe1`) et se termine sur une **métrique
> mesurable** et reproductible. Les chiffres sans source de la première version (« conviction
> 35 % → 95 % », « batterie 100 % → 0 % en 2 h », « cache de 20 à 40 Go ») ont été retirés : ils
> seront remplacés par des mesures, ou pas du tout.

---

## Ce que la première version supposait à tort

| Affirmation de la v1 | Ce que dit le code | Conséquence |
|---|---|---|
| « Le Plan 3 est terminé à 100 % » | Le streaming YAML/Markdown sous budget mémoire n'est livré qu'à moitié : `serde_yaml` 0.9 met en mémoire tous les événements d'un document, le seul plafond est la limite de 384 KB par fichier, et le Markdown est lu en entier (`docs/quality.md`, revue de la 3.5). | Jalon 4.9 ci-dessous. |
| L'agent peut voir un graphe « moitié ancienne branche, moitié nouvelle » pendant un rebase (4.2) | Le snapshot est publié de façon atomique (`ArcSwap`, `AppState::install_snapshot`, `crates/mesh-core/src/state.rs`) : aucun lecteur ne voit un graphe à moitié modifié. | Le vrai risque est ailleurs, voir 4.2. |
| Le « `GovernanceEngine` » masque les JWT et les chaînes à haute entropie, ce qui casse les fixtures de test (4.3) | La rédaction se fait **par nom de clé** (`PropertyRegistry::is_sensitive_key`, `crates/mesh-core/src/properties.rs:243`) sur des lignes `clé: valeur`. `let token = "eyJ…"` n'est pas masqué (une clé qui contient un espace est ignorée). Aucune détection d'entropie n'existe. | Jalon rétrogradé en 4.8, à mesurer avant toute action. |
| Sockets morts dans `/tmp/mesh-<hash>.sock`, aucun nettoyage (4.4) | Les sockets vivent dans `$XDG_RUNTIME_DIR/mesh/` ou `~/.cache/mesh/` (`crates/mesh-core/src/socket.rs:48-66`). `cleanup_stale_socket` existe (`socket.rs:97`), ainsi que `mesh-mcp doctor`. Le watchdog de la 3.3 arrête les démons orphelins. Les dossiers cache/audit sont en `0700`, les fichiers en `0600`. | Jalon réduit aux manques réels. |
| Il faut créer `mesh-mcp stats` (4.7) | Existe déjà (`crates/mesh-server/src/cli/stats.rs`), à partir de l'audit local. | Jalon recentré sur des métriques honnêtes. |
| Le cache SQLite n'a ni `busy_timeout` ni WAL (4.1) | `busy_timeout` 5 s, `journal_mode=WAL` et `synchronous=NORMAL` sont en place (`crates/mesh-core/src/index_cache.rs:100-110`). Ce qui manque vraiment : l'éviction et le partitionnement. | Jalon réduit. |
| « `meshd` < 80 Mo » comme objectif du pilote (4.7) | Mesuré : 49 MB à 5 000 fichiers, mais **630 à 830 MB de pic et 15 à 17 s de boot à 200 000 fichiers** (`docs/quality.md`, « Plan 3 closeout »). | Il faut fixer la taille de dépôt cible, voir 4.3. |

Et un risque produit absent de la v1 : **le recall gRPC TypeScript est à 53,8 %** sur le corpus
golden `otel-demo` (tous les appels `frontend → *` écrits avec `@grpc/grpc-js` sont ratés). Un pilote
vendu sur l'analyse d'impact gRPC ne peut pas partir avec la moitié des appelants invisibles, sans le
dire. Voir 4.5.

---

## Vue d'ensemble

| Jalon | Sujet | Risque traité | Critère de sortie mesurable |
|---|---|---|---|
| **4.0** | CI sur les PR empilées + merge du Plan 3 | Code jamais passé en CI (`ci.yml` ne tourne que sur `main`) | CI verte sur chaque PR avant son merge ; Plan 3 mergé |
| **4.1** | Diagnostics d'indexation visibles par l'agent | Fichiers ignorés en silence (taille, binaire, parse) | L'agent voit la raison de chaque fichier exclu de son scope |
| **4.2** | Watcher macOS sans polling + rechargement pendant les opérations Git | Batterie, snapshots mêlant deux branches | CPU au repos mesuré ; aucun snapshot installé pendant un checkout |
| **4.3** | Profil de taille cible et budgets par taille | Objectifs impossibles à tenir à 200k | Budgets documentés et tenus pour 3 tailles de corpus |
| **4.4** | Cache SQLite : un fichier par workspace, quota, éviction | Croissance disque sans limite, contention entre fenêtres | Taille bornée sous charge mesurée ; 0 `SQLITE_BUSY` à 4 processus |
| **4.5** | Recall gRPC TypeScript (`@grpc/grpc-js`) | Analyse d'impact aveugle sur le frontend | Golden `otel-demo` recall ≥ 90 %, précision ≥ 87,5 % |
| **4.6** | Analyse d'impact v2 (matrice), puis ruptures wire-format | Valeur différenciante | Test d'intégration multi-langages ; spec de la version « avant » arrêtée |
| **4.7** | `doctor --fix`, droits du socket, contrôle de version | Pannes au démarrage de l'IDE | Artefacts morts nettoyés par un test d'intégration |
| **4.8** | Pilote : installation, `stats` honnête, scorecard | Déploiement sans données | Rapport de pilote fondé sur des mesures, pas des estimations |
| **4.9** | Mémoire YAML/Markdown sous budget | Reste du Plan 3 | Pic mémoire borné sur un corpus de specs volumineuses |
| **4.10** | Sandbox réseau du démon (Linux d'abord) | Exigence RSSI | Test prouvant `EPERM` sur une connexion sortante |
| **4.11** | Masquage des secrets dans les fixtures | Faux positifs de rédaction | Seulement si un cas réel est mesuré |

L'ordre est celui du tableau. 4.1 à 4.3 sont indépendants et peuvent avancer en parallèle
après 4.0.

---

## Spécifications

### 4.0 — CI sur les PR empilées, puis merge du Plan 3

**Constat.** `.github/workflows/ci.yml` se déclenche sur `push`/`pull_request` vers `main` seulement.
Les PR du Plan 3 (#26 → #31) ciblaient chacune la branche précédente et n'ont eu de CI qu'une fois
mergées dans `main` (verte en 6.0.0). Le merge du Plan 3 est fait ; il reste le filtre.

**Travail.**
- Déclencher `ci.yml` sur toutes les PR (`pull_request:` sans filtre de branche), ou au moins sur
  `p*/**`.
- Merger le Plan 3 dans l'ordre (#26 → #31) une fois chaque PR verte, en rebasant la suivante.

**Sortie.** CI verte sur chaque PR au moment de son merge ; `main` en 6.0.0.

---

### 4.1 — Diagnostics d'indexation visibles par l'agent

**Constat.** `IndexHealth` compte déjà chaque rejet (taille, garde lexicale, binaire, échec de
parse) dans le snapshot (`crates/mesh-core/src/state.rs:34`, `crates/mesh-core/src/health.rs`), mais
aucun outil ne le remonte. Un `smart_search` qui ne trouve rien parce que le fichier dépasse 384 KB
ressemble à un bug.

**Travail.**
- Garder, en plus des compteurs, la liste bornée des chemins rejetés et leur raison (par exemple les
  100 premiers par racine, triés pour rester déterministes).
- Quand un outil travaille sur un scope qui contient des fichiers rejetés, ajouter une note en fin de
  résultat : chemin, raison, taille, et quoi faire (« lire le fichier directement »).
- Exposer le même résumé dans `mesh-mcp doctor`.

**Sortie.** Test d'intégration : un fichier de 450 KB dans le scope → `smart_search` renvoie les
autres résultats **et** la note sur ce fichier. Sortie toujours sous 48 KB.

---

### 4.2 — Watcher macOS sans polling, et rechargement différé pendant les opérations Git

**Constat.**
- Sur macOS, `FileWatcherService::MAX_WATCHED_DIRS = 200` (`crates/mesh-core/src/watcher.rs:69`).
  Au-delà, le service bascule sur `PollWatcher` (`watcher.rs:192`), qui re-parcourt l'arbre en
  continu. La cause, documentée dans `watcher.rs:60` : `FsEventWatcher` redémarre son flux à chaque
  `.watch()`, donc un enregistrement dossier par dossier gèle le démarrage.
- Un rechargement lancé pendant un `git checkout` lit un disque en cours de modification. Le
  snapshot produit est cohérent en mémoire, mais il mélange des fichiers des deux branches. Il est
  alors installé comme n'importe quel autre.

**Travail.**
- macOS : une seule surveillance FSEvents **récursive par racine** configurée (les racines sont
  peu nombreuses), et le filtrage `.gitignore`/`exclude_patterns` appliqué en mémoire à chaque
  événement, avec le matcher de la 3.3. Le `PollWatcher` ne sert plus que si FSEvents refuse de
  démarrer (volume réseau, par exemple). Linux et Windows ne changent pas.
- Git : surveiller `.git/HEAD`, `.git/index.lock`, `.git/rebase-merge`, `.git/rebase-apply`. Tant
  qu'une opération est en cours, accumuler les chemins sans recharger. Quand l'opération se termine
  (verrou disparu, `HEAD` stable pendant la fenêtre de debounce), faire **un seul** rechargement
  groupé. Un `index.lock` plus vieux que 30 s sans processus `git` vivant est considéré comme
  orphelin : on logue un avertissement et on reprend.
- Pendant l'attente, les outils continuent de répondre avec le dernier snapshot complet, et
  ajoutent une note dans le **texte** du résultat (« opération Git en cours, index de la génération
  N »). Pas de champ `meta` ajouté à la réponse JSON-RPC, qui ne serait pas standard, et pas
  d'attente imposée à chaque appel d'outil.

**Sortie.**
- CPU au repos de `meshd`, mesuré sur un dépôt de plus de 200 dossiers pendant 10 minutes :
  chiffre publié avant et après (cible : aucun réveil périodique visible).
- Script `scripts/test_git_storm.sh` : checkout de 3 000 fichiers pendant des requêtes
  `smart_search` → aucun snapshot installé pendant le checkout, un seul après, avec une empreinte
  égale à celle d'une indexation à froid de la branche d'arrivée (via `mesh-mcp graph --format
  fingerprint`).

---

### 4.3 — Taille de dépôt cible et budgets par taille

**Constat.** Les budgets nightly (`scripts/bench/budgets.json` : boot 3 s, RSS 300 MB, p50/p95
300/800 ms) sont calibrés sur 5 000 fichiers et tenus à cette taille. À 200 000 fichiers, les mesures
de la 6.0.0 donnent 15 à 17 s de boot et 630 à 830 MB de pic. Le parsing en parallèle et le graphe
gardé en mémoire dominent, et le Plan 3 n'y a pas touché.

**Travail.**
- Arrêter avec l'équipe pilote la taille des dépôts réels : nombre de fichiers et de racines,
  à partir de 2 ou 3 dépôts de l'entreprise.
- Définir des budgets par taille (par exemple 5k / 50k / 200k fichiers) et les tenir dans le bench
  nightly. Les corpus doivent être générés **hors d'un chemin ignoré par git** : sinon le watcher
  ne voit pas les modifications et `reload_ms` vaut `None` (voir « Plan 3 closeout »).
- Si la taille cible dépasse ce que tiennent les budgets, profiler et réduire le pic du parsing
  (taille du lot rayon, libération des arbres tree-sitter, résidence du graphe).

**Sortie.** Budgets écrits pour chaque taille, bench nightly vert à chacune.

---

### 4.4 — Cache SQLite : un fichier par workspace, quota, éviction

**Constat.** `~/.cache/mesh-mcp/index-cache.db` est un fichier unique pour tous les workspaces, sans
aucune éviction : il ne fait que grandir. Le `busy_timeout`, le WAL et `synchronous=NORMAL` sont déjà
en place (`index_cache.rs:100-110`). Aucune mesure de croissance réelle n'existe encore.

**Travail.**
- Mesurer d'abord : taille par entrée et croissance sur une semaine de travail simulée (changements
  de branche, rebases).
- Un fichier par workspace : `~/.cache/mesh-mcp/workspaces/<id>/index-cache.db`, avec le même
  `workspace_id` que le socket.
- Colonne `last_accessed_at`, `PRAGMA auto_vacuum = INCREMENTAL` sur les nouvelles bases, et un
  quota configurable (`[cache] max_size_mb`). À l'ouverture, si le quota est dépassé, supprimer les
  entrées les moins récentes jusqu'à 80 % du quota, puis `incremental_vacuum`.
- Migration : l'ancien fichier global est ignoré puis supprimé par `doctor --fix` (4.7).

**Sortie.**
- Test : insertion au-delà d'un quota de test → taille ramenée sous le seuil.
- Test : 4 processus indexant 4 workspaces en même temps → 0 `SQLITE_BUSY`.
- Croissance mesurée publiée dans `docs/quality.md`.

---

### 4.5 — Recall gRPC TypeScript (`@grpc/grpc-js`)

**Constat.** `scripts/golden/score.py otel-demo` : **87,5 % de précision / 53,8 % de recall**,
inchangé en 6.0.0. Les 6 arêtes `frontend → *` sont toutes manquées. Le frontend construit ses
clients avec `new AdServiceClient(ADDR, ChannelCredentials.createInsecure())`
(`src/frontend/gateways/rpc/*.gateway.ts`), un idiome que l'extracteur TypeScript ne reconnaît pas.
Il ne gère que `ClientGrpc.getService<XServiceClient>(...)` (NestJS). S'y ajoute une arête parasite,
`checkout → health` (le service gRPC de health-check), qui n'est pas filtrée.

**Travail.**
- Reconnaître la construction `new <X>Client(...)` quand `<X>Client` vient d'un module généré
  depuis un `.proto` (import de `*_grpc_pb`, de `@grpc/grpc-js`, ou d'un stub `*.ts` généré).
- Filtrer le service `grpc.health.v1.Health` comme infrastructure.

**Sortie.** Golden `otel-demo` : recall ≥ 90 %, précision ≥ 87,5 %. Les deux autres corpus restent
à 100 %/100 %. Le ratchet (`--fail-under-*`) est branché dans la CI (4.0).

---

### 4.6 — Analyse d'impact v2, puis ruptures wire-format

**Constat.** C'est la partie différenciante du produit, mais la v1 mélangeait deux chantiers de
taille très différente.

**6a — Matrice d'impact** (`analyze_impact`) :
- Classer chaque élément impacté : `EXTERNE` (un autre service ou une autre racine l'appelle)
  ou `INTERNE` (même service).
- Chaque ligne porte la confiance de l'arête (exacte, heuristique, ambiguë), déjà connue du graphe.
- La catégorie « trou de couverture de test » de la v1 est **retirée** : le graphe ne relie pas les
  tests au code qu'ils couvrent, donc elle serait inventée.
- Sortie : tableau Markdown compact sous 48 KB. Test d'intégration Go + Rust + proto.

**6b — Ruptures wire-format** (`analyze_grpc`) : réutilisation d'un numéro de champ, changement de
type, champ supprimé sans `reserved`. **Préalable à trancher avant tout code** : ces règles exigent
une version « avant » du `.proto`. La v1 ne disait pas d'où elle venait. Options : une ref git de base
passée en argument (`base: "origin/main"`), lue avec `git show` sans toucher au working tree ; ou un
snapshot des messages proto gardé dans le cache de la 4.4. Choisir, puis spécifier.

**Sortie.** 6a : test d'intégration, latence publiée. 6b : spec de la version « avant » validée, puis
tests sur les trois règles.

---

### 4.7 — `doctor --fix`, droits du socket, contrôle de version

**Constat.** Déjà en place : sockets hors de `/tmp`, nettoyage des sockets orphelins
(`cleanup_stale_socket`), watchdog d'inactivité et de démarrage (3.3), droits `0700`/`0600` sur le
cache et l'audit. **Manquent réellement** :
- droits explicites `0700` sur le dossier du socket et `0600` sur le socket lui-même (aucun
  `set_permissions` sur ce chemin aujourd'hui) ;
- `doctor --fix` ;
- la détection d'un `meshd` d'une autre version que le CLI (le socket est déjà indexé par la version
  du binaire, mais un démon ancien peut rester vivant) ;
- `PRAGMA quick_check` sur le cache et l'audit.

**Travail.** Ces quatre points, plus un `doctor --json` pour les scripts d'installation.

**Sortie.** Test d'intégration : socket orphelin + base de cache corrompue + démon d'une autre
version → `doctor --fix` les nettoie ; un second `doctor` est vert.

---

### 4.8 — Pilote : installation, `stats` honnête, scorecard

**Constat.** `mesh-mcp stats` existe déjà (appels par outil, taux d'erreur, scopes consultés). La v1
voulait y ajouter des « tokens économisés » calculés par rapport à des lectures que l'agent n'a
jamais faites. C'est une estimation invérifiable, qui gonflerait le ROI devant une direction.

**Travail.**
- Script d'installation idempotent (`scripts/install_pilot.sh`) : binaire dans `~/.local/bin`, puis
  `mesh-mcp init --write-ide-config`, qui existe déjà, **avec confirmation** avant de modifier une
  configuration d'IDE.
- `stats` : latences p50/p95 par outil, taux d'`isError`, taux de hit du cache d'index (4.4),
  redémarrages du démon. Rien n'est estimé, tout vient de l'audit local.
- Mesure du gain : sur un échantillon de tâches réelles, comparer les tokens consommés avec et sans
  MeshMCP (A/B), plutôt que de les estimer.
- Scorecard de fin de pilote : écrite avec l'équipe pilote avant le démarrage, à partir des mesures
  ci-dessus.

**Sortie.** Rapport de pilote dont chaque chiffre renvoie à une mesure.

---

### 4.9 — Mémoire YAML/Markdown sous budget (reste du Plan 3)

**Constat.** Voir la revue de la 3.5 dans `docs/quality.md` : `serde_yaml` 0.9 met en mémoire les
événements d'un document entier ; le Markdown est lu en entier et chaque section reste en mémoire.

**Travail.** Mesurer d'abord le pic mémoire sur un corpus de specs OpenAPI/AsyncAPI volumineuses et
de docs Markdown. Si le pic dépasse le budget de la taille cible (4.3), passer à un parseur YAML
événementiel et à un découpage Markdown ligne par ligne, avec un budget explicite par fichier.

**Sortie.** Pic mémoire mesuré et borné sur ce corpus.

---

### 4.10 — Sandbox réseau du démon (Linux d'abord)

**Constat.** Aucune dépendance réseau dans le code (ni `reqwest`, ni `hyper`, ni `TcpStream`,
vérifié). Un RSSI demandera tout de même une garantie au niveau du noyau.

**Travail.**
- Linux : après le bind du socket Unix, filtre seccomp qui refuse `socket(AF_INET|AF_INET6, ...)`,
  plus `PR_SET_NO_NEW_PRIVS`. `AF_UNIX` reste autorisé.
- macOS : `sandbox_init` est une API dépréciée. À étudier séparément, sans promesse dans ce plan.
- Écrire les limites : seul `meshd` est confiné. Le proxy `mesh-mcp run` et `mesh-mcp graph --open`
  (qui lance un navigateur) ne le sont pas.

**Sortie.** `tests/security_airgap.rs` (Linux) : une connexion TCP tentée depuis `meshd` après
confinement échoue avec `EPERM`.

---

### 4.11 — Masquage des secrets dans les fixtures (seulement si mesuré)

**Constat.** Le masquage par nom de clé peut toucher une fixture de test du type `api_token:
sk_test_…` dans un YAML. Aucun cas réel n'a été rapporté. Désactiver le masquage sous `tests/` a un
coût de sécurité : de vrais secrets finissent aussi dans des fixtures.

**Travail.** Pendant le pilote, compter les masquages par type de chemin (via l'audit, 4.8). Ne
concevoir une exception que si des faux positifs gênants sont observés. Elle devrait alors être
opt-in et limitée à des chemins explicites, sans désactivation globale.

**Sortie.** Décision fondée sur les données du pilote.

---

## Décisions verrouillées (revue du 2026-09-26)

Une revue externe du plan a soulevé 7 points ouverts. Chacun a été vérifié dans le code avant
d'être tranché : quatre sont retenus tels quels, trois sont retenus avec correction.

| Jalon | Question | Décision | Vérification / correction |
|---|---|---|---|
| **4.0** | Filtre de déclenchement de la CI | `pull_request: branches: ["main", "p*/**"]`. La matrice existante ne change pas : tests sur ubuntu/macos/windows, déterminisme et release sur ubuntu/macos (`ci.yml:47`, `:76`, `:103`). | Le Plan 3 a été mergé le 2026-09-26 : `main` passe en CI en 6.0.0 (run vert sur `1f1ffe1`). Pour les prochaines piles, GitHub reroute d'office une PR empilée vers `main` quand sa branche de base est supprimée après merge. Avec des merge commits, pas de doublons. Seul un squash-merge demande `git rebase --onto main <ancienne-base> <branche>`, puis un push de la branche rebasée. |
| **4.1** | Diagnostics sous 48 KB | La note de diagnostic est **réservée avant** la troncature des résultats : les résultats se partagent `48 KB − note`, et la note est toujours ajoutée intacte en dernier. Liste bornée (par exemple 20 fichiers, puis « et N autres »). | Même mécanique que la note de troncature de la 3.6 (comptée avant la coupe). **Filtrée sur le scope** de la requête : un `smart_search` sur `crates/mesh-core` ne cite jamais un rejet de `crates/mesh-server`. |
| **4.2** | Sous-modules et worktrees Git | Pour chaque racine, résoudre le vrai répertoire Git : si `.git` est un **fichier** (sous-module, ou `git worktree`), suivre son pointeur `gitdir:`. Les verrous (`index.lock`, `HEAD`, `rebase-*`) sont surveillés dans ce répertoire. | Verrou orphelin : `index.lock` plus vieux que 30 s **et** aucun processus `git` de l'utilisateur en cours (lecture de la table des processus, sans `lsof`, trop coûteux). On logue un avertissement et on reprend. |
| **4.4** | Moment de l'éviction | Contrôle du quota **à l'ouverture et après chaque rechargement** ayant écrit plus de 100 entrées dans le cache. | Un démon qui tourne toute la semaine n'est ouvert qu'une fois : sans contrôle en cours de route, la base grossit jusqu'au redémarrage suivant. |
| **4.5** | Détection `@grpc/grpc-js` | Une `new_expression` `new <X>Client(...)` crée une arête gRPC **si et seulement si** (1) `<X>Client` est importé dans le fichier et (2) `<X>` (ou `<X>Service`) correspond à un service gRPC **déclaré dans le graphe** (proto indexé). | **Correction** : otel-demo n'importe pas depuis un `*_grpc_pb`, mais depuis `'../../protos/demo'` (fichier `demo.ts` généré par ts-proto, `src/frontend/gateways/rpc/*.gateway.ts:5`). Une heuristique sur le chemin d'import (« contient `proto` ») est trop large : `prototype`, `protocol`… La résolution contre les services déclarés est ce qui empêche un `new FooClient()` quelconque de devenir une arête. |
| **4.6b** | Version « avant » du `.proto` | Git uniquement : argument `base: Option<String>`, contenu lu par `git show <base>:<chemin>`, en mémoire, sans écriture disque ni persistance SQLite. | **Correction du défaut** : `base` vaut par défaut le merge-base de `HEAD` et de la branche par défaut distante (`origin/HEAD`) ; à défaut, `HEAD`, ce qui compare le working tree au dernier commit. **Pas `HEAD~1`**, qui comparerait au commit précédent au lieu de la branche de base. Fichier absent dans `base` : aucune rupture, fichier nouveau. Hors dépôt Git : `isError` explicite. |
| **4.7** | Configuration d'IDE | **Fusion non destructive** : lire le JSON existant, n'ajouter ou remplacer que l'entrée `mesh-mcp`, et garder tous les autres serveurs. Si le fichier est malformé, ne rien écrire et le signaler. | **Bug existant, à corriger sans attendre 4.7** : `init --write-ide-config` réécrit aujourd'hui `.cursor/mcp.json` et `.vscode/mcp.json` avec un contenu neuf (`crates/mesh-server/src/cli/init.rs:303`, `:322`) et **efface les autres serveurs MCP** de l'utilisateur. À vérifier au passage : la clé racine attendue par VS Code (`servers` et non `mcpServers`). |
| **4.10** | Seccomp et threads | Filtre posé avec `SECCOMP_FILTER_FLAG_TSYNC`, qui l'applique à tous les threads existants : workers et pool bloquant Tokio, pool rayon, threads de `notify`. | `meshd` démarre sous `#[tokio::main]` (`crates/mesh-daemon/src/main.rs:56`) et doit binder son socket avant de se confiner : poser le filtre avant le runtime n'est donc pas praticable sans restructurer `main`. Test : une connexion TCP tentée depuis un thread créé **avant** le confinement échoue aussi avec `EPERM`. |

Palier de budgets de 4.3 retenu : **5k / 50k / 200k fichiers**, chacun avec ses propres budgets
de boot, de pic mémoire et de latence. Le palier 200k part des mesures de la 6.0.0 (15 à 17 s,
630 à 830 MB), pas de l'objectif de 80 MB de la première version.

## Branches et dépendances

| Jalon | Branche suggérée | Dépend de |
|---|---|---|
| 4.0 | `p3/4.0-ci-and-plan3-merge` | — |
| 4.1 | `p3/4.1-index-diagnostics` | 4.0 |
| 4.2 | `p3/4.2-macos-fsevents-git-fencing` | 4.0 |
| 4.3 | `p3/4.3-size-profiles-and-budgets` | 4.0 |
| 4.4 | `p3/4.4-sqlite-cache-quota` | 4.0 |
| 4.5 | `p3/4.5-ts-grpc-recall` | 4.0 |
| 4.6 | `p3/4.6a-impact-matrix`, `p3/4.6b-wire-format` | 4.5 (6a), décision de spec (6b) |
| 4.7 | `p3/4.7-doctor-fix` | 4.4 |
| 4.8 | `p3/4.8-pilot` | 4.1, 4.2, 4.3, 4.7 |
| 4.9 | `p3/4.9-yaml-md-memory` | 4.3 |
| 4.10 | `p3/4.10-linux-network-sandbox` | 4.7 |
| 4.11 | — | données du pilote (4.8) |

Méthode inchangée par rapport aux Plans 1 à 3 : une étape = une PR ; un constat mesuré avant le
correctif ; des chiffres avant/après sur le même corpus ; une review adversariale dont les findings
sont corrigés avant le merge.
