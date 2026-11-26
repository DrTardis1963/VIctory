# VI-BR-PORT: a versão para placas de 8 GB

Camada de compatibilidade que roda no Windows, de forma nativa, a versão de
**Xbox Series X|S** (plataforma *Scarlett*, jogos feitos com o **GDK**) de um
título, a partir dos arquivos do próprio usuário. O código x86-64 do console
roda sem alteração dentro do nosso processo; por baixo dele ficam as bibliotecas
do sistema do console reescritas (HLE) e um back-end Direct3D 12 que recompila
os shaders da GPU do console (GFX10/RDNA2) para DXIL.

Este fork tem um alvo só: **placa NVIDIA de 8 GB com ondas de 32 lanes**
(testado numa RTX 4060 Ti) ligada a um **monitor 1080p**.

**Este repositório não contém arquivos, código ou recursos do jogo.** É preciso
ter a sua própria cópia. Nada aqui tem ligação com o desenvolvedor ou a
editora do jogo, nem é aprovado por eles.

Origem: [VIctory](https://github.com/Marvisak/VIctory) →
[fxceless/victory-fork](https://github.com/fxceless/victory-fork) →
[avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX) → este fork.
O README da árvore do avatarmanz está em [docs/README-avatarmanz.md](docs/README-avatarmanz.md)
e o do fxceless em [docs/README-fxceless.md](docs/README-fxceless.md).

## Onde está (2026-11-25)

A base passou a ser a árvore do avatarmanz: na mesma placa de 8 GB ela mostra
rostos, pele, cabelo e luz bem melhores que a nossa base anterior. Por cima dela
vieram os nossos ajustes para NVIDIA de 8 GB.

| | Medido (RTX 4060 Ti 8 GB, i7-13700K, 1080p) |
|---|---|
| Imagem, perfil Series X | rostos e pele com textura, luz coerente nas cutscenes |
| fps, perfil Series X, release | ~5,6 (a placa sozinha leva ~105 ms por quadro) |
| Memória da placa, Series X | no limite; na prisão passa do orçamento e a textura borra |
| Perfil Series S | composição de planos portada, em teste |
| Termos e telas iniciais | passam sozinhos (`scripts` do usuário) |

**Não é jogável ainda.** O alvo é 15 fps ou mais nas cutscenes e na prisão, nítido.

## O que este fork acrescenta à base

- **`VICTORY_RT_MISS=1`**: o teste de raio não acerta nada; sem a estrutura de ray
  tracing montada, as sombras de luzes locais saíam de lixo (rosto preto ou com "purpurina").
- **Assado de decoração do rosto reconhecido pelo nome** (`PS_DecorationBlend_*`,
  `PS_BakeDecorations_*`, 51 programas): a lista de 4 hashes deixava de fora o que
  faz os rostos da cutscene, e o rosto saía preto.
- **Listas de memória alterada compactadas em vez de apagadas** quando enchem: apagar
  deixava texturas com bytes velhos (o vídeo da intro "voltava quadros").
- **Composição de planos do present** (Series S: o mundo e a interface em planos separados).
- **Patches do título também no release** (`VICTORY_UPSCALER=6`: o upscaler do próprio
  jogo no Series S, já que os passes do FidelityFX não têm DXIL aqui).
- **Pipelines**: pilha de 64 MB para o compilador do driver da NVIDIA, compute começando
  na criação, espera de 500 ms só nos 60 primeiros quadros (8 ms depois) e uma pipeline
  atrasada segurando só o primeiro quadro que a pede.
- **wave64 em placas de 32 lanes**: programas que pediriam ondas de 64 lanes usam a forma larga.
- **DirectStorage** grava em páginas somente leitura como o motor do console.
- `VICTORY_NO_WATCHDOG`, `VICTORY_NULL_MAGENTA` (diagnóstico) e o teto de 262.144
  faixas remapeadas.

## Como rodar

```powershell
copy victory.example.toml victory.toml      # e aponte game_dir para a sua cópia
cargo build --locked --release -p victory
target\release\victory.exe shaders <os arquivos de shaders do jogo>
target\release\victory.exe run <o executável do jogo> --console series-x --execute
```

Configuração usada nas medições acima: `VICTORY_RT_MISS=1`, `VICTORY_RELAY=1`,
`VICTORY_THIN=0`, `VICTORY_DISPLAY_MODE=1920x1080`, `VICTORY_PIPELINE_BOOK=1`,
`VICTORY_MEMORY_CARD_MB=1024`, `VICTORY_TEXTURE_ROOM_PERCENT=85`. Não ligue
`VICTORY_THIN=1` numa NVIDIA (o fps cai a menos da metade).

## Licença

GPL-3.0-or-later, como o VIctory. Ver [LICENSE](LICENSE).
