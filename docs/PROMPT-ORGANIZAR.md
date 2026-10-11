# Prompt para outra sessão do Claude Code organizar o brAIn

Abra o Claude Code em `~/workspace` e cole o bloco abaixo. **Feche o brAIn antes** (ele
reescreve o `config.json` ao salvar).

````
Quero que você organize meu ambiente no brAIn (launcher de projetos do Claude Code). Leia
`~/workspace/areas/Pessoal/Organização/brAIn/docs/COMO-USAR.md` primeiro. O brAIn guarda tudo em
`~/.config/brain/config.json`. Trabalhe em duas fases.

FASE 1 — DIAGNÓSTICO (só leitura, nada é alterado)
1. Percorra `~/workspace` (ignore dot-dirs, node_modules, target, .venv, _arquivo,
   chave-emergencia; NUNCA abra conteúdo de chave-emergencia, .env ou segredos).
2. Para cada pasta de 1º/2º nível, classifique em UMA categoria do brAIn:
   projetos, educacao, clientes (consumidoras) ou documentacoes, agentes, diario (fontes).
   Use os CONTEXT.md e CLAUDE.md como evidência.
3. Liste candidatos a base de contexto: docs canônicas (ex.: `~/workspace/docs`), diário/registros
   e agentes (`~/.claude_copy/agents`, nunca `~/.claude`). Para cada base diga a quais categorias
   ou pastas ela deve valer.
4. Aponte pastas mal posicionadas, duplicadas ou sem categoria clara — sem mover nada.
5. Entregue uma tabela: pasta | categoria sugerida | motivo | bases que recebe.
PARE e espere minha aprovação.

FASE 2 — APLICAR (só depois de eu aprovar)
1. Faça backup: `cp -n ~/.config/brain/config.json ~/.config/brain/config.json.bak-$(date +%Y%m%d-%H%M%S)`.
2. Edite o config com `jq` ou um script (nunca à mão sobre JSON grande), mantendo o schema:
   - `projects["<caminho absoluto>"] = {"region":"<id>","tags":[],"dependsOn":[],"hidden":false,"displayName":null}`
   - `bases[] = {"id":"<único>","path":"<absoluto>","label":"<nome>","appliesTo":["<pasta>"],"appliesToRegions":["<id>"]}`
   - não altere `regions` nem `terminal` sem eu pedir; no máximo 20 bases.
3. Valide: JSON válido (`jq . arquivo`), caminhos existentes, nenhuma base proibida
   (`~`, `/home`, `~/.ssh`, `~/.config`, `~/.claude`, `~/.aws`, `~/.gnupg`), e que o brAIn lê
   o arquivo (abra o app e confira, ou me peça).
4. Se eu aprovar mover pastas: copy-then-validate, e a origem vai para a Lixeira com `gio trash`.

REGRAS
- Nunca `rm`, `mv`, sobrescrita, `git clean` ou qualquer ação destrutiva; siga o CLAUDE.md do hub.
- Não toque em `~/.claude`, `~/.ssh`, `~/.config` (exceto `~/.config/brain/config.json` na fase 2).
- Não imprima conteúdo de segredos. Em dúvida, pergunte.
- Termine com: o que mudou, onde está o backup e como reverter (`cp` do backup de volta, com o app fechado).
````
