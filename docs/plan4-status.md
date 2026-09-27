# Plan 4 — état d'avancement et passation

> Fichier de passation de l'orchestrateur. Il complète `docs/plan4-enterprise-readiness.md` (le cahier
> des charges) et `CLAUDE.md`. Il est mis à jour à chaque merge. Une session qui reprend le Plan 4
> (locale ou cloud) le lit en premier.
>
> **Dernière mise à jour** : 2026-09-27 — `main` = `d81ca50`.

---

## 1. État des jalons

| Jalon | État | PR / branche | Tête | Reste à faire |
|---|---|---|---|---|
| 4.0 filtre CI PR empilées | ✅ mergé | #36 | — | — |
| 4.12a déterminisme CI macOS | ✅ mergé | #38 | — | — |
| 4.5 recall gRPC TS + golden CI | ✅ mergé | #39 | — | — |
| 4.4 cache par workspace, quota | ✅ mergé | #40 (`d64d0ac`) | — | **bug intermittent, voir §2.1** |
| 4.6b ruptures wire-format | ✅ mergé | #41 (`d81ca50`) | — | — |
| 4.4-fix éviction intermittente | 🔧 en cours | `p3/4.4-fix-eviction` (pas encore poussée) | — | cause racine, correctif, preuve N/N, PR |
| 4.2 watcher macOS + Git | 🔍 en review | #43 `p3/4.2-watcher-git` | `45b39c7` | fin de review adversariale, puis merge |
| 4.9 mémoire YAML/Markdown | 🔍 en review | #42 `p3/4.9-yaml-md-memory` | `9af9ecc` | fin de review adversariale, puis merge |
| 4.1 diagnostics d'indexation | ⏳ à faire (débloqué) | — | — | tout |
| 4.6a matrice d'impact | ⏳ à faire | — | — | tout (notes §3.2) |
| 4.3 budgets 5k/50k/200k | ⏳ à faire | — | — | tout ; machine calme requise (§4) |
| 4.7 `doctor --fix`, socket, version | ⏳ à faire (après 4.1) | — | — | tout |
| 4.12b–e dette (audit, fences, homonymes, outil inconnu) | ⏳ à faire | — | — | tout |
| 4.12f empreinte sans chemins absolus | ⏳ à faire (après 4.9) | — | — | tout (§3.3) |
| 4.10 sandbox réseau Linux | ⏳ à faire (après 4.7) | — | — | tout |
| 4.8 install pilote + `stats` | ⏳ à faire (après 4.7) | — | — | partie agent seulement (§5) |
| 4.11 masquage des secrets | 🧑 humain | — | — | aucun code (§5) |
| Clôture 6.1.0 | ⏳ | — | — | plan §5, plus §6 ci-dessous |

Légende : ✅ mergé · 🔍 PR prête, review en cours · 🔧 correctif en cours · ⏳ pas commencé · 🧑 humain.

---

## 2. Problèmes ouverts sur `main` ou dans les PR

### 2.1 Éviction du cache intermittente (4.4, sur `main`) — MAJOR
`crates/mesh-server/tests/index_cache_quota.rs:340`, test `partially_evicted_cache_indexes_exactly_like_no_cache`.
Il a échoué une fois en CI macOS (run 36304190590, tentative 1, job 108577531925) avec
`CacheStats { hits: 0, misses: 6000, errors: 0, evicted: 6000 }`, puis il est passé à la relance.
C'est exactement le symptôme d'origine : l'éviction vide tout le cache au lieu de redescendre à 80 %.
Le correctif de la review (départage par `payload_rowid`, schéma v4, transaction `IMMEDIATE`) n'est
donc pas déterministe. Pistes : la taille mesurée (base + WAL) ne baisse pas tant que le WAL n'est pas
checkpointé, et la boucle continue d'évincer ; `incremental_vacuum` ; horodatages égaux ; timing
macOS. **Exigence** : cause prouvée, correctif sans assouplir le test (≤ 80 % du quota **et** entrées
conservées), N/N runs verts sous charge.

### 2.2 Coût mémoire de `props` (4.9, PR #42)
Il reste entre 12× et 25× la taille du fichier, au-dessus du seuil de 4×. Le coût vient du
`PropertyRegistry` (deux maps, chacune avec ses copies de clés), pas du parseur. Il est borné à 8×
de sortie et documenté dans `docs/quality.md`. Réduire le registre est hors périmètre de la 4.9.

### 2.3 Limites connues, documentées, non bloquantes
- 4.5 : un client TS construit au niveau module et utilisé seulement depuis une `function` de premier
  niveau ou une arrow `const` n'a pas d'arête. Les imports CommonJS `require()` ne sont pas gérés.
- 4.6b : sans `base` explicite, un dossier hors dépôt Git ou un `git` absent donne une note
  « skipped » au lieu de `isError`. Écart assumé, documenté dans `docs/mcp-tools.md`. Chaque appel
  coûte environ 42 ms de plus.
- 4.2 : une seule attente couvre toutes les racines. Sous Windows, il n'y a pas de vérification des
  processus (seul le plafond de 60 s libère un verrou orphelin). Les `.gitignore` imbriqués ne sont
  pas filtrés en mémoire.
- 4.12a : l'empreinte dépend du dossier de checkout (voir 4.12f).

---

## 3. Notes de conception pour les jalons à faire

### 3.1 4.1 — diagnostics d'indexation
4.4 a modifié `crates/mesh-core/src/state.rs` (ajout de `AppState::index_cache`, `open_index_cache`) :
repartir de `main`. 4.2 a ajouté 7 lignes dans `ToolRegistry::invoke` (`tools/mod.rs`) pour la note
« opération Git en cours » ; la note de rejet de 4.1 doit coexister avec elle.

### 3.2 4.6a — matrice d'impact (exploration déjà faite)
- `examples/polyglot-shop` couvre Go, Rust et proto : pas besoin de nouvelle fixture.
- `analyze_impact` s'appuie sur `ContractGraph::analyze_impact_with_depth`
  (`crates/mesh-core/src/contracts.rs`, ~l.1246) et `format_impact_flow` (`markdown.rs`, ~l.338).
  Il ne traite aujourd'hui que l'asynchrone : il faut l'étendre aux méthodes proto et gRPC, en
  réutilisant la résolution handlers/clients d'`analyze_grpc`.
- Il n'existe pas de champ « service » : seulement `repo_id` (la racine) et `package`, qui n'est pas
  fiable. Règle proposée : le service propriétaire est la racine des handlers qui implémentent la
  méthode, sinon la racine du `.proto`. EXTERNE = autre racine, INTERNE = même racine.
- Dans `markdown.rs`, n'ajouter qu'un formateur ; ne pas toucher `format_search_entry` (4.12c).

### 3.3 4.12f — empreinte indépendante du dossier (nouveau jalon, décidé en cours de plan)
L'empreinte inclut des chemins absolus : `canonical_node_key` utilise `node.file_path`
(`contracts.rs`, ~l.1596), et on trouve la même chose dans `docs.rs` (~l.164) et `properties.rs`
(~l.235). Le même contenu dans deux dossiers donne deux empreintes, et ubuntu diffère de macOS en CI.
Correctif : chemins relatifs à la racine (`repo_id` + chemin relatif) dans les trois
`canonical_lines`, avec un test sur deux dossiers de checkout. Touche les fichiers de 4.9 : à faire
après le merge de #42.

### 3.4 4.8 — partie agent (réponse de l'utilisateur)
Le pilote est mené par l'utilisateur **après** la release 6.1.0. Le jalon côté agent est clos quand :
(1) `scripts/install_pilot.sh` est testé et idempotent (deux runs = même état) ; (2) `stats` calcule
les p50/p95 réels, le taux d'`isError`, le taux de hit du cache d'index (compteurs
`PersistentIndexCache::stats()` déjà exposés par 4.4) et les redémarrages du démon, sur un audit de
fixture ; (3) **un template de scorecard documente le protocole de mesure A/B**.

---

## 4. Décisions de l'utilisateur (à respecter)

- **4.3** : pas de tailles de dépôts d'entreprise. On garde les paliers synthétiques 5k / 50k / 200k,
  complétés par des dépôts réels (`~/bench-repos` en local ; dans le cloud, cloner des dépôts publics
  équivalents, par exemple kubernetes ou vscode). Les budgets doivent être mesurés **sur une machine
  calme**, pas pendant des compilations parallèles. Dans le cloud, les chiffres sont des chiffres
  Linux : les étiqueter comme tels.
- **4.8** : le pilote est humain, après 6.1.0 ; périmètre agent en §3.4.
- **4.11** : aucun code ; on compte les masquages par type de chemin pendant le pilote.
- **Barème des reviews** : review adversariale complète pour 4.1, 4.2, 4.7, 4.9 et 4.10 ; une seule
  review groupée pour 4.12b à 4.12e ; pas de review par sous-agent pour un changement trivial
  (l'orchestrateur vérifie lui-même). Les findings MINOR et au-delà sont corrigés avant le merge.
- **4.12f** est ajouté au plan (§3.3).
- Personne ne merge sans l'accord de l'utilisateur. Jamais de force-push, jamais de push sur `main`.

---

## 5. Règles d'environnement

**En local (Mac de l'utilisateur)** :
- `cd` est aliasé vers un zoxide cassé : utiliser `builtin cd`, des chemins absolus ou `git -C`.
- Toujours `--release` : en debug, rustc se bloque plusieurs minutes.
- Un `meshd` personnel tourne sur le socket par défaut : ne jamais le tuer. Tests avec `HOME` isolé
  et `MESH_SOCKET_PATH` court.
- Ne pas parcourir le disque hors du dépôt, de ses worktrees, de `target/scale-bench`,
  `~/.cache/mesh-golden` et `~/bench-repos`.

**Dans le cloud (Linux)** :
- Pas de `meshd` personnel ni de zoxide, mais garder `HOME` isolé dans les tests.
- Corpus golden : `scripts/golden/fetch.sh` (clones dans `~/.cache/mesh-golden`), puis
  `scripts/golden/score.py <repo>` avec le binaire release en tête de `PATH`.
- Mesures macOS impossibles (4.2 est déjà mesuré). 4.10 (seccomp) se teste nativement.
- Budget limité : une seule session, pas de sous-agents en parallèle, tests ciblés, et la CI
  (`gh pr checks <n> --watch`) fait la validation complète.

**Partout** :
- Chaque PR : une branche depuis `main`, un fragment `changelog.d/4.x-<slug>.md`, jamais
  `CHANGELOG.md` ni la version. `docs/quality.md` : ajout d'une section datée seulement, avant
  « What's NOT measured yet ». `Cargo.lock` : `cargo update -w`, jamais à la main.
- Après chaque merge, intégrer `main` dans les branches encore ouvertes (merge, pas de rebase forcé).
- Commits en anglais, terminés par la ligne `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`
  (plus la ligne `Claude-Session:` si l'outil la fournit). Descriptions de PR terminées par
  `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.
- Aucun chiffre inventé : une mesure manquante se mesure ou se déclare manquante.
- Heredocs toujours quotés (`<<'EOF'`) quand le texte contient des backticks.

---

## 6. Ordre de reprise recommandé

1. Merger ce qui est prêt : #43 (4.2) et #42 (4.9) quand leurs reviews sont vertes.
2. Correctif de l'éviction intermittente (§2.1).
3. 4.1, puis sa review complète.
4. 4.12b–e, avec une review groupée.
5. 4.7, puis sa review complète.
6. 4.10, puis sa review complète.
7. 4.6a.
8. 4.8 (partie agent).
9. 4.12f.
10. 4.3, sur une machine calme.
11. Clôture (plan §5). En plus du plan : nettoyer dans `docs/quality.md` les lignes périmées par 4.5
    (les puces otel-demo de « What's NOT measured yet », la phrase « not yet wired in » de « Ratchet
    policy ») ; consolider `changelog.d/` ; lister ce qui reste humain (4.8 pilote, 4.11, 4.3 réel).

---

## 7. Prompt de reprise (à coller dans une nouvelle session)

```text
Tu reprends l'orchestration du Plan 4 de MeshMCP (dépôt causalmesh).
Lis dans l'ordre : CLAUDE.md, docs/plan4-enterprise-readiness.md, docs/plan4-status.md.
Si docs/plan4-status.md n'est pas sur main, prends la version la plus récente sur la branche
distante : git fetch origin p3/plan4-status && git show origin/p3/plan4-status:docs/plan4-status.md
docs/plan4-status.md fait foi pour l'état, les décisions de l'utilisateur, les règles
d'environnement (section « cloud » si tu n'es pas sur le Mac de l'utilisateur) et l'ordre de reprise.
Commence par : git fetch ; gh pr list ; vérifier que l'état réel correspond au §1, et le corriger
sinon. Puis reprends au premier point non fait du §6.
Budget limité : une seule session, pas de sous-agents en parallèle, tests ciblés en --release, la CI
fait la validation complète. Après chaque merge, mets à jour docs/plan4-status.md dans sa PR.
Ne merge jamais sans mon accord ; jamais de force-push ni de push sur main.
```
