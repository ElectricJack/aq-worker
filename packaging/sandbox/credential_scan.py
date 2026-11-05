#!/usr/bin/env python3
"""Fail-closed OCI layer scanner. No archive member is extracted to a host path."""
import gzip
import hashlib
import json
from pathlib import Path
import posixpath
import re
import tarfile
import tempfile

MAX_ARCHIVE = 32 * 1024**3
MAX_EXPANDED = 64 * 1024**3
MAX_ENTRIES = 1_000_000
MAX_JSON = 1024 * 1024
DIGEST = re.compile(r'sha256:[0-9a-f]{64}\Z')
TOKEN_PATTERN = re.compile(
    # The public Serde capability label persistent-disk-during-attempt sits in
    # .rodata against whichever literal codegen places next; that neighbor is
    # build layout, not content. Exclude only the sk- inside this exact label,
    # without consuming bytes: every adjacent byte, later tokens and the
    # independent URL/assignment checks still run. The docker-local label key
    # aq.disk-limit-bytes (SBX20) is excluded the same way, for the same reason.
    rb'(?:sk-(?!(?<=persistent-disk-)during-attempt)(?!(?<=aq\.disk-)limit-bytes)'
    rb'(?:proj-|ant-)?[A-Za-z0-9_-]{20,}'
    rb'|gh[pousr]_[A-Za-z0-9]{20,}'
    rb'|github_pat_[A-Za-z0-9_]{20,}|AIza[A-Za-z0-9_-]{30,}|AKIA[A-Z0-9]{16}'
    rb'|https?://[^\s/:\x22\x27]+:[^\s/@\x22\x27]+@'
    rb'|eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{16,}'
    rb'|(?i:authorization\s*:\s*(?:bearer|basic)\s+[A-Za-z0-9_+/=-]{4,})'
    rb'|-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----)')
ASSIGNMENT = re.compile(rb'(?<![A-Za-z0-9_])([A-Za-z][A-Za-z0-9_]{0,127})[\x22\x27]?\s*=\s*[\x22\x27]?([A-Za-z0-9_+/.-]{4,})')
JSON_ASSIGNMENT = re.compile(rb'"([A-Za-z][A-Za-z0-9_]{0,127})"\s*:\s*"([A-Za-z0-9_+/=.:,-]{4,4096})"')
CREDENTIAL_LABEL = re.compile(r'(?:API_KEY|TOKEN|SECRET|PASSWORD|CREDENTIALS?|ACCESS_KEY_ID|SECRET_ACCESS_KEY|OPENAI_KEY)\Z')

# Reviewed public distribution bytes containing format constants, self-test
# vectors or source examples. This catalog is part of the installed scanner,
# never image/config input. See docs/implementation/C3b-public-constants.md and
# its independently downloaded upstream archive/member verification evidence.
# Identity is the *entire* original file, not its name, prefix or ELF format.
PUBLIC_FILE_CONSTANTS = {
    # etc/ssl/openssl.cnf
    '7ae8cae2e64856b34c80276deb1dcf60f76da27bc1e00382201ba7bb7dc33311': 12332,
    # opt/aq/harnesses/codex/codex
    'b8a11fedf24b2dfb59a70c9e52b030f9a508dcd49cd2dd0d41956f4c785156b8': 319346888,
    # opt/aq/harnesses/claude/claude
    '708c77b204db30086ff6f1ed34577bda03e1be74ced80557cbf7b54249ebf0bd': 337814032,
    # opt/aq/harnesses/opencode/opencode
    '77b2cfe4b97df6f15c3673b22100b9f79c711f25ecb9bf513bb82526b15d24fa': 185690240,
    # opt/aq/harnesses/gemini/README.md
    '7235196f55461732c246c20ce9996cc56ba2e52641f183ccf4e6493bd93c1c94': 12107,
    # opt/aq/harnesses/gemini/bundle/bundled/chrome-devtools-mcp.mjs
    'b7bccc7a27f2b2d5689ec6096f9d73bd6d12ec172cd838373c804baf3fb6657e': 11629199,
    # opt/aq/harnesses/gemini/bundle/bundled/third_party/index.js
    '162f5d1e7d1be664945cd0107ebb23bd822b99795b896b5d42a2ff084a0f5769': 7995195,
    # opt/aq/harnesses/gemini/bundle/bundled/third_party/issue-descriptions/corsRedirectContainsCredentials.md
    '0099164610ffb6ccf2001d3d5f1c94bbe7bad671c5623c50e001c9b07e3fba3b': 377,
    # opt/aq/harnesses/gemini/bundle/chunk-5RNHNFYV.js
    '76ea4cb08171d4636c418a934f9c9f8076a0da1740fbdb0a8b2fb435d8cc82c6': 2680262,
    # opt/aq/harnesses/gemini/bundle/chunk-664ZODQF.js
    '0e5e41b8af7ca2ddd1bc3509c7276559d48a99705d6716b6d45c4c64b4f9168c': 123520,
    # opt/aq/harnesses/gemini/bundle/chunk-BW65QRZA.js
    'bde9bcb2673611604fcfad60830f95783dcd4d01ceaf48e784fe4359adad0f18': 1632350,
    # opt/aq/harnesses/gemini/bundle/chunk-CJFBYU4J.js
    '4eaad86c398b16126d3b1bc116b97b984d3e2ef3e84bcea77adc3c60400ab609': 2680262,
    # opt/aq/harnesses/gemini/bundle/chunk-IUUIT4SU.js
    '878469f1440495db8e40b033dde88ab96c0ee7d9e40c3deb4cee91dde1c8031a': 56507,
    # opt/aq/harnesses/gemini/bundle/chunk-LAR4JEC7.js
    '10250bdeaea7afde7f8107109b97cce273a91e0e2ae9fed41e3219f372f0af04': 16799212,
    # opt/aq/harnesses/gemini/bundle/chunk-NGQU3QTQ.js
    '37e3e8cebdcfe5dabfe3cea87c0ce6022f02cbbd89a543046fb4d6ab81ab5a32': 16762057,
    # opt/aq/harnesses/gemini/bundle/chunk-O7MGOE2I.js
    'a9972316ce0974111ca8906ee6148dbf89f19b8aa032a17f463db03b917d4110': 16799232,
    # opt/aq/harnesses/gemini/bundle/chunk-P62CI7AR.js
    'c8662875f269e738964ec2c81f2655e89418fef2bf36b004d0681a8c4cb0beab': 1632346,
    # opt/aq/harnesses/gemini/bundle/chunk-VO6PFWLY.js
    '8a03e94c879e78ac6068b88f0de54102fb93d4cf44bbf774dbb83dce3a2edbe7': 1632346,
    # opt/aq/harnesses/gemini/bundle/chunk-XETHQLRU.js
    'b4a23beb48145efb0d8266f010b9c5a9f48cfa2d462eec88a0da7baa7a306c48': 2680349,
    # opt/aq/harnesses/gemini/bundle/devtoolsService-6V46WMXX.js
    'ad0e88b28c28e3261f59d9eb0fef95406955e4b3b9dccb8b0774fcfd541e3354': 149131,
    # opt/aq/harnesses/gemini/bundle/devtoolsService-E74ZMDMM.js
    '5e465cc5344af92715b81678f471d5b882db986de2d315b49d1555e53896406e': 149134,
    # opt/aq/harnesses/gemini/bundle/devtoolsService-EFJXZRFD.js
    '0c6f92c2455ec3054ed786e3f2dca3cb0f2d13f947159c9285b464f366d04c2f': 149131,
    # opt/aq/harnesses/gemini/bundle/docs/cli/cli-reference.md
    'ec376db5857fe8dda715ce38575370c33aa03390925b2d7024f0642021525545': 16563,
    # opt/aq/harnesses/gemini/bundle/docs/cli/telemetry.md
    'c731cf1f5403c2164f1164dc1c9581782e324c16cc6c4fedb51716978d45c133': 29634,
    # opt/aq/harnesses/gemini/bundle/docs/cli/tutorials/mcp-setup.md
    '9a4633c905689c79d06e1e3fe59236ccc76010bce5ef6da38b0ff41e5f14a4b0': 3254,
    # opt/aq/harnesses/gemini/bundle/docs/get-started/authentication.mdx
    '8a2eb3910788d931b0892f2cc5d188f6f0dd4602349767db307df2b242bbfd5f': 15046,
    # opt/aq/harnesses/gemini/bundle/docs/ide-integration/ide-companion-spec.md
    '143b49bbf7817cc5d6c8c3c6e3e1b255cb2a0f4c710a039d756cf8028c68cd91': 10821,
    # opt/aq/harnesses/gemini/bundle/docs/reference/configuration.md
    '4b86e73ee6da5b18ec0515c3fbf12b9b72fb07fbb545fa93362507596cbc64f4': 102685,
    # opt/aq/harnesses/gemini/bundle/docs/tools/mcp-server.md
    '9e4608980e3adf50d39c6b336536829257f45f48a83f5e7e677a622929693b46': 44112,
    # opt/aq/harnesses/gemini/bundle/gemini-4JPKPLQN.js
    '445fdc1c0c535e2017aa891a055e65534a5ccd662c24b0c7030783f6d5b19463': 691780,
    # opt/aq/harnesses/gemini/bundle/gemini-QWDTSJTS.js
    '3ba01a4e7e18cea5d27ee401f5f12d315971769c1c1f686d598f0a9a4d0a7491': 691770,
    # opt/aq/harnesses/gemini/bundle/gemini-VF5FS3OM.js
    '5ba06a235e29c4abd306765329ffacaaac82d719d27c2696c8e077f238076730': 691371,
    # opt/aq/harnesses/gemini/bundle/interactiveCli-F2YYMYTP.js
    '2f692696d0c62966fcd77802cb3a0653ce734c1a16c0aad5f35ad0db09c1f440': 1337852,
    # opt/aq/harnesses/gemini/bundle/interactiveCli-FQ3YKGOG.js
    'a1a7db6b7386f91c969649d162bdad614256613ff6d7d928e7160d5fed6ab663': 1337852,
    # opt/aq/harnesses/gemini/bundle/interactiveCli-QJZZISQQ.js
    'd6ae7102d5624f04a89fbce297fbd1afcd371308175a4de4c53cfdc58f83a536': 1336760,
    # opt/aq/harnesses/gemini/bundle/oauth2-provider-6UCP23AQ.js
    '24f152aa4015ed4ebad1985650f91d88e9ce91a70e6edfdceda29372cc778272': 10252,
    # opt/aq/harnesses/gemini/bundle/oauth2-provider-C65IJMNQ.js
    '371203d2fbc754c20364b5669d9548f6853734194cff80c39f47cfa3c0e5269f': 10570,
    # opt/aq/harnesses/gemini/bundle/oauth2-provider-H2KV36NU.js
    '36f5885c436d7f914b3fc5fdc4669306729d272191c9c0dbe49afbbd05e4a946': 10570,
    # opt/aq/harnesses/gemini/bundle/src-XZYPU6PJ.js
    'e5c8c17f156442e50819612bde280e458f9a07286ea8384e725871baa7a22210': 352121,
    # opt/aq/harnesses/gemini/bundle/tree-sitter-bash-LMFXTVOP.js
    '8327848fcd92d2f4737422c797749a650fd21261b0c28d6ec7cead86e2276c2d': 1841544,
    # opt/aq/node/bin/node
    '8142d37c6f2f372ef040419e7a111a6baf17df89f8078d02447d6c639ae20c1d': 121509208,
    # opt/aq/node/lib/node_modules/corepack/dist/lib/corepack.cjs
    '3dafc27aaa5c71358f0515241cce7492b24b03a9d93be6ab940f306e3febfa0f': 962955,
    # opt/aq/node/lib/node_modules/npm/docs/content/using-npm/config.md
    '4793d88688041eb7660ffc8fa249a0e41f2dad7bbdcd2441073424b154b8bb02': 46442,
    # opt/aq/node/lib/node_modules/npm/docs/output/using-npm/config.html
    'bd9b76debdd6e652e985b399d5d8941b05cd4706d7bddb69ffb691836f0fa63d': 75798,
    # opt/aq/node/lib/node_modules/npm/lib/commands/profile.js
    'adbdcde3b2ef2fc35495a03060ffa4c0d151e039f704126d51b1c20c5a8c8b17': 10819,
    # opt/aq/node/lib/node_modules/npm/lib/commands/token.js
    'ae27e6d635831267466cc821f5e0a18a9fdd754ceb6b90eb28fe4c999b173fea': 6160,
    # opt/aq/node/lib/node_modules/npm/lib/utils/auth.js
    'c13178cde84b223f94cf06e7a7a1c7e4fbcba9dc44e15a1acc731d8c6969cd7a': 3291,
    # opt/aq/node/lib/node_modules/npm/man/man7/config.7
    'f318905e29e58fc3fef4ea159309e29f6cef92d7c57ed1ec957ad0b7910d0098': 54612,
    # opt/aq/node/lib/node_modules/npm/node_modules/@npmcli/config/lib/definitions/definitions.js
    '83b2068320f1a131c96658940e80522ec1f29150b338ce40eccf486e0ee05900': 73793,
    # opt/aq/node/lib/node_modules/npm/node_modules/@npmcli/config/lib/index.js
    'aa283456b6e3c72396b0bc8230d31072665f753d5b80e04efa556713339adbad': 28888,
    # opt/aq/node/lib/node_modules/npm/node_modules/@pkgjs/parseargs/examples/limit-long-syntax.js
    '92d07fcfe098ec3e8545ddc388871c54f167933ce34b3f25f1f3525c027c5c29': 1071,
    # opt/aq/node/lib/node_modules/npm/node_modules/diff/dist/diff.js
    '43decd0ad71f8e4b312d1c95ee5b1ea446cebb006c6fcb718dd81d0a7bfc8278': 53658,
    # opt/aq/node/lib/node_modules/npm/node_modules/diff/lib/diff/json.js
    'e9ef76b36e5a19263d199ac35871a15aa2b33c00479192b71f6c8f46b91a1661': 12937,
    # opt/aq/node/lib/node_modules/npm/node_modules/diff/lib/index.mjs
    '5b16b23087ed45b8f5718ad6834bee324dbb860755c7c40d8b9dc594075d58a7': 50001,
    # opt/aq/node/lib/node_modules/npm/node_modules/jsonparse/jsonparse.js
    '8fee0da896c802ccc73d8f0db740625ad0dd6b403a7bc5c86f4afdd17ba8941e': 15570,
    # opt/aq/node/lib/node_modules/npm/node_modules/node-gyp/gyp/pylib/packaging/_parser.py
    'e4384aff3609138538cb34a4804053e05eed4f6c59e8f931e204912b2bd79de4': 10382,
    # opt/aq/node/lib/node_modules/npm/node_modules/node-gyp/gyp/pylib/packaging/_tokenizer.py
    '6a50ad6f05e138502614667a050fb0093485a11009db3fb2b087fbfff31327f9': 5292,
    # opt/aq/node/lib/node_modules/npm/node_modules/npm-profile/lib/index.js
    'd91acad36065d6a64bc95c0ed6a0f103c3224b59d7985938f22a12f481503721': 6869,
    # opt/aq/node/lib/node_modules/npm/node_modules/npm-registry-fetch/lib/auth.js
    'fc50e1fa09f89b75eebfb632d7c4063a26881ec10791246ae4b30cfc47a8bd84': 5374,
    # opt/aq/node/lib/node_modules/npm/node_modules/postcss-selector-parser/dist/parser.js
    '594e5cabc1baa4b1b49d3aa614138a44183a8ae7f8d87291281bbc344c9b38fb': 39081,
    # opt/aq/node/lib/node_modules/npm/node_modules/sigstore/dist/config.js
    '8e2067981d52721988a681543e9e04973ce30e40ea8c8ce65632f7d059089634': 4320,
    # opt/aq/node/lib/node_modules/npm/node_modules/sigstore/node_modules/@sigstore/sign/dist/signer/fulcio/index.js
    'cc4124686b7b6d10f3fe1128f22714920852b639f939f58ff290701821ece3ec': 3485,
    # opt/aq/node/lib/node_modules/npm/node_modules/socks/build/client/socksclient.js
    '4b473feca4862b5c66ec0efa39ea2d40d07c845886b6081813b07d489fabd897': 35626,
    # opt/aq/node/lib/node_modules/npm/node_modules/spdx-expression-parse/scan.js
    'a3b23fce07fb28a267ef35cd0b8230bff6eb68700578a315982fc7924636538b': 2774,
    # opt/aq/node/lib/node_modules/npm/node_modules/validate-npm-package-license/node_modules/spdx-expression-parse/scan.js
    'b3c4dcd0af9b6e72cd1c56335db37447657114e8201271653bf35c5bfe7afba6': 2754,
    # opt/git/libexec/git-core/git-p4
    'b6c106bbf7e36b5c67c98b6f8e9aaa3863a953908f3f509bdfd9eae2bb93dd79': 172865,
    # opt/git/share/perl5/Git.pm
    'f797504db05192c01300f6eecdc70d46e7f066c44fc43487a2908ee8cc54a3ab': 48387,
    # opt/node/bin/node
    'd918479037f14dc30c8d21df3767a010fe710685373baec7edf680ca8c3edbee': 129491488,
    # opt/node/lib/node_modules/corepack/dist/lib/corepack.cjs
    'bafd892df44cd70740e23e5d43eeea934b4f261a9eaff3637dac29bdea74d829': 963574,
    # opt/node/lib/node_modules/npm/docs/content/using-npm/config.md
    '89fc5f1fb236c8be718aa45e91022dd43ce73b6028e87e96d285ee9f8d11d182': 47321,
    # opt/node/lib/node_modules/npm/docs/output/using-npm/config.html
    '21551173569fc409b3f5842d3e68298808d0c9f1cc50b7cda46e5b213c361e70': 77205,
    # opt/node/lib/node_modules/npm/lib/commands/profile.js
    '3cfe31f01747fcf285f47e7173c6b3686a21e4fc9ccad69687dce69711773c09': 10696,
    # opt/node/lib/node_modules/npm/lib/commands/token.js
    'c7e0512e654c8fc9fc234c235c6d10d46aad511976abb847a81507f7ce019538': 6066,
    # opt/node/lib/node_modules/npm/lib/utils/oidc.js
    'c24d567d07e771ce3cb38b1cfbcd6f564da646904e8fb7b653f85c391a7b9f57': 7098,
    # opt/node/lib/node_modules/npm/man/man7/config.7
    'a3f0c35cc25a58946ba505e8a1ff791b35491beca7270eda5eb8fb633480c570': 55662,
    # opt/node/lib/node_modules/npm/node_modules/@npmcli/config/lib/definitions/definitions.js
    '139887f10ade68539e130584636d00fc63c5a29cbe35708dee9ffdb39c872998': 74850,
    # opt/node/lib/node_modules/npm/node_modules/@npmcli/config/lib/index.js
    '45ff414dec518bb1c5745cbcb840c8fcfdd1285b4becc7207456c07323a88ce7': 30544,
    # opt/pnpm/dist/pnpm.cjs
    'ca969fbb32c08be8395c744c64e131c2ba2c93067e1b483171dbc6ed439e1143': 8632942,
    # opt/pnpm/dist/worker.js
    '7b8aba01c22e65e9d1998fd1e30dc9722e4c315f80c87bcc54826d9ddb944e59': 424672,
    # opt/rust/bin/cargo
    'a73b2c25573d251489101c0d8f19ad3702eb9761166de5ed8437b472b6c038ce': 42878680,
    # opt/rust/bin/clippy-driver
    '4b6976de2e62e12adaa5e6ef190b1d3f3fbf062e0f8ea5f4014fca45fb918a66': 20959320,
    # opt/rust/lib/rustlib/x86_64-unknown-linux-gnu/bin/wasm-component-ld
    '053399b53d6802732e584d20b02eb4a09b798d54d2828c8839c4f320a604aaa1': 5165592,
    # usr/include/c++/12/barrier
    '67cec2a2dcd392951ec014ae7d07148255103861c92d3f233beaeeec21b5fd93': 7750,
    # usr/include/x86_64-linux-gnu/bits/socket.h
    'dc5e124f23281bca31e1c9118b5e1fa39b9b94520e07043ecc29838b804aa8b1': 13575,
    # usr/lib/python3.11/cgitb.py
    '401f791a56480b5b53e207d71a904e6352c5e7fc3f7cc9124c75bd1c59a5c011': 12421,
    # usr/lib/python3.11/email/_header_value_parser.py
    '2bc1acfa3c7f8d2c419173ec2d986261d04d85fc1adba2bf39bf00b88aba1491': 108140,
    # usr/lib/python3.11/email/generator.py
    'de706af1044f8b6021263b02cc7325248eeec19614134b21fd2f916f3c56f19c': 21404,
    # usr/lib/python3.11/functools.py
    '6d80018a3fa9fed44c5b78f881987ee3ab3fb1238b077b4e6dcb4b76da7db73b': 38413,
    # usr/lib/python3.11/hashlib.py
    'e2bffb462e4d43e6637b9450e259e8ba2a56626ba3037d68aa1cee68b3f61d4a': 11765,
    # usr/lib/python3.11/imaplib.py
    'dc3be105fb6b3e28074c4dbe0d4177c8445a803b5a935f3c4bbfe77701259c39': 54865,
    # usr/lib/python3.11/inspect.py
    '75147eb66270082c23edf22ddcfa953c00cea398867b91cd8a4b167b1cf30ede': 124193,
    # usr/lib/python3.11/lib-dynload/_ssl.cpython-311-x86_64-linux-gnu.so
    '78e47b6cc76acace7d0f5d1942d503334847602a14dab8304a0cf7895055f3ec': 212272,
    # usr/lib/python3.11/logging/handlers.py
    'a9041e01c48e06b0c3c73a1b5ab9348aea94001abafe3427ffdc0e89e9e81ed8': 62332,
    # usr/lib/python3.11/multiprocessing/managers.py
    '314c299639bf01f9c30dc4c8ee9327c022665f279194cd8435626eb3c9d6a031': 47683,
    # usr/lib/python3.11/netrc.py
    '6394e8e650d04c26ed20b6058d5e93119f544ef1bd00aefb942297d2e2f6c7e1': 6929,
    # usr/lib/python3.11/nntplib.py
    '6a76a94b951b273aa87335d7c9c4d7273e4c59485c784b057f681443b32d9004': 41087,
    # usr/lib/python3.11/plistlib.py
    'e4fcade34b0783ffaf01dad1d4f89ad59942db94e6a6bb992ff50d879203f855': 28489,
    # usr/lib/python3.11/poplib.py
    '7b341e6adb4e4341b211a221a7224849ea415fc1e39c014b05ae7d9a566819f4': 15198,
    # usr/lib/python3.11/shlex.py
    '42ab6060f316e121e374e6621d8c1c98b8db323903c3df289a810c45a8ae46a7': 13501,
    # usr/lib/python3.11/smtplib.py
    'b154e27998d0ef0b32fcc665299cdb30883ffe039ede3c4faac3a9db6618d35d': 45418,
    # usr/lib/python3.11/test/support/__init__.py
    '022f0b59a434e93b3906fa929819b695b3be59a52c0bb75d40a96e34a4f1a3f5': 76431,
    # usr/lib/python3.11/tokenize.py
    '79aa94ebe1761811ea9b2da2ada409a8cb9abd7c6eac241d0c181a36684bb609': 26336,
    # usr/lib/python3.11/urllib/parse.py
    'd2bf673217a06bf4e450f355b9843482265664a3d6cfcfa00bc31944c9a8deb1': 44707,
    # usr/lib/python3.11/urllib/request.py
    'f3348315e7c74cba5596e5dd9e4092fee2742218730d882ec216f57c6c173536': 102724,
    # usr/lib/python3.11/xml/etree/ElementPath.py
    'ae8a80a8b51567b4f0965481682705e70c73dd6bfa145283f630d6833f1b4975': 13997,
    # usr/lib/x86_64-linux-gnu/libgnutls.so.30.34.3
    '779b25d20249988bea2c1aa6bbeb218f5ae7ea8a9d30ce4f54ea37372965cc4b': 2209528,
    # usr/lib/x86_64-linux-gnu/libssh2.so.1.0.1
    'e481655791a9b75f4d5957e40101d7d0b5d9c13a18d1ca233731d03365ad0aec': 260552,
    # usr/lib/x86_64-linux-gnu/libunistring.so.2.2.0
    'bc5951aa3d6eaba20ff9688efa3420dc95785aae3709ec48ff6df46d6f409ee5': 1792040,
    # usr/share/perl/5.36.0/CPAN/HTTP/Credentials.pm
    'e45a6b41d7caebdb5f23d3b4f219adf64a214e946b082304b845d7b449a8aba5': 2633,
    # usr/share/perl/5.36.0/HTTP/Tiny.pm
    'a28408da12eb2e156377d63dcb891a27c0d16d79d07dd9794810ee3c8cc33cf8': 82093,
    # usr/share/perl/5.36.0/Pod/Simple/PullParser.pm
    'b14d10e0bb584f4b2211528be99d9a9e440dd9bd486249e74a1de8d6b9aee862': 25729,
    # usr/share/perl/5.36.0/TAP/Parser.pm
    'aa1b59263b4806f791b58530bc4936c893c2646e0984ea28a810c2cc171e3e5a': 52200,
}


def secret_shaped(value):
    data = value.encode() if isinstance(value, str) else value
    return bool(TOKEN_PATTERN.search(data) or any(
        CREDENTIAL_LABEL.search(match[1].decode('ascii').upper())
        for match in ASSIGNMENT.finditer(data)) or any(
        CREDENTIAL_LABEL.search(match[1].decode('ascii').upper())
        # Public protocol format identifier embedded in the Codex mapper,
        # not an authentication value. Keep this exception exact and JSON-only.
        and (match[1], match[2]) != (b'credentials', b'private-codex-chatgpt-auth-json')
        for match in JSON_ASSIGNMENT.finditer(data)))


def json_bytes(data):
    if len(data) > MAX_JSON:
        raise ValueError('JSON size limit')
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError('duplicate JSON field')
            result[key] = value
        return result
    try:
        return json.loads(data, object_pairs_hook=pairs)
    except (UnicodeError, json.JSONDecodeError, RecursionError) as error:
        raise ValueError('invalid JSON') from error


def safe_path(name):
    if len(name) > 4096 or '\\' in name or '\x00' in name or name.startswith('/'):
        raise ValueError('unsafe archive path')
    parts = name.split('/')
    if '..' in parts:
        raise ValueError('unsafe archive path')
    return '/'.join(part for part in parts if part not in ('', '.'))


def credential_path(name):
    parts = name.lower().strip('/').split('/')
    base = parts[-1]
    return (base in {'auth.json', '.git-credentials', '.netrc', '.claude.json',
                     'application_default_credentials.json', 'credentials.db',
                     'oauth_creds.json', '.credentials.json'}
            or any(p in {'.codex', '.claude', '.gemini', '.aws', '.azure'} for p in parts[:-1])
            or any('/' + p + '/' in '/' + name.lower() + '/' for p in
                   ['.config/gcloud', '.config/gh', '.config/doctl', '.config/oci', '.config/ascii/boat'])
            or ('.ssh' in parts[:-1] and base.startswith('id_'))
            or ('.docker' in parts[:-1] and base == 'config.json'))


def credential_env(name):
    name = name.upper()
    return (any(word in name for word in ['API_KEY', 'ACCESS_TOKEN', 'AUTH_TOKEN',
                                         'SECRET', 'PASSWORD', 'CREDENTIAL'])
            or name.endswith('_TOKEN')
            or name in {'AWS_ACCESS_KEY_ID', 'AWS_SESSION_TOKEN', 'GITHUB_TOKEN',
                        'GH_TOKEN', 'GITLAB_TOKEN', 'HF_TOKEN', 'NPM_TOKEN',
                        'AZURE_OPENAI_KEY', 'GOOGLE_API_KEY', 'GEMINI_API_KEY'})


def credential_link_path(name):
    # Conservatively refuse aliases of credential directories rather than
    # interpreting a later layer through a mutable symlink graph.
    return credential_path(name) or any(part in {
        '.codex', '.claude', '.gemini', '.ssh', '.docker', '.aws', '.azure', '.config'
    } for part in name.lower().strip('/').split('/'))


def scan_content(stream, capture=False):
    tail = b''
    collected = bytearray() if capture else None
    digest = hashlib.sha256()
    size = 0
    credential_found = False
    while data := stream.read(1024 * 1024):
        digest.update(data)
        size += len(data)
        credential_found = credential_found or secret_shaped(tail + data)
        tail = (tail + data)[-4096:]
        if capture:
            if len(collected) + len(data) > MAX_JSON:
                raise ValueError('bundle manifest size limit')
            collected.extend(data)
    # A public constant is established only after hashing every original byte.
    # Any edit, appended credential, truncation or unknown package version
    # refuses. Sensitive paths/links/config/history are checked separately;
    # captured bundle metadata always uses the strict content policy.
    if credential_found and (capture or PUBLIC_FILE_CONSTANTS.get(digest.hexdigest()) != size):
        raise ValueError('credential content detected')
    return bytes(collected) if capture else None


def validate_bundle(value):
    if not isinstance(value, dict) or value.get('schema_version') != 1 or value.get('platform') != 'linux/amd64':
        raise ValueError('unsupported bundle manifest')
    harnesses = value.get('harnesses')
    if not isinstance(harnesses, list) or not 1 <= len(harnesses) <= 32:
        raise ValueError('invalid bundle harnesses')
    names = set()
    for item in [value.get(k) for k in ['runner', 'session_host', 'aq_git', 'node']] + harnesses:
        if not isinstance(item, dict):
            raise ValueError('invalid bundle artifact')
        for key in ['name', 'version']:
            if not isinstance(item.get(key), str) or not re.fullmatch(r'[A-Za-z0-9_.+@-]{1,128}', item[key]):
                raise ValueError('invalid bundle artifact metadata')
        path = item.get('path', '')
        if not path.startswith('/opt/aq/') or safe_path(path[1:]) != path[1:]:
            raise ValueError('invalid bundle artifact path')
        if not re.fullmatch(r'[0-9a-f]{64}', item.get('sha256', '')) or type(item.get('bytes')) is not int or item['bytes'] <= 0:
            raise ValueError('invalid bundle artifact digest')
    for item in harnesses:
        if item['name'] in names:
            raise ValueError('duplicate harness')
        names.add(item['name'])
    return value


def _scan_oci(path):
    path = Path(path)
    if path.stat().st_size > MAX_ARCHIVE:
        raise ValueError('OCI archive size limit')
    with tempfile.TemporaryDirectory(prefix='aq-oci-scan-') as directory:
        blobs = {}
        metadata = {}
        total = 0
        entries = 0
        seen = set()
        with tarfile.open(path, mode='r|*') as archive:
            for item in archive:
                entries += 1
                if entries > MAX_ENTRIES:
                    raise ValueError('OCI entry limit')
                name = safe_path(item.name)
                if name in seen:
                    raise ValueError('duplicate OCI member')
                seen.add(name)
                if item.isdir():
                    continue
                if not item.isfile():
                    raise ValueError('invalid OCI member')
                total += item.size
                if total > MAX_ARCHIVE:
                    raise ValueError('OCI expanded size limit')
                stream = archive.extractfile(item)
                if name in {'oci-layout', 'index.json'}:
                    metadata[name] = json_bytes(stream.read(MAX_JSON + 1))
                elif re.fullmatch(r'blobs/sha256/[0-9a-f]{64}', name):
                    dest = Path(directory) / name.rsplit('/', 1)[1]
                    digest = hashlib.sha256()
                    with dest.open('xb') as output:
                        while data := stream.read(1024 * 1024):
                            digest.update(data)
                            output.write(data)
                    if dest.stat().st_size != item.size or digest.hexdigest() != dest.name:
                        raise ValueError('OCI blob digest mismatch')
                    blobs['sha256:' + dest.name] = dest
                else:
                    raise ValueError('unsupported OCI archive member')
        if metadata.get('oci-layout') != {'imageLayoutVersion': '1.0.0'}:
            raise ValueError('invalid OCI layout')
        used = set()
        def descriptor(desc):
            if not isinstance(desc, dict) or not DIGEST.fullmatch(desc.get('digest', '')):
                raise ValueError('invalid OCI descriptor')
            digest = desc['digest']
            if digest not in blobs or type(desc.get('size')) is not int or desc['size'] != blobs[digest].stat().st_size:
                raise ValueError('OCI descriptor size or blob missing')
            used.add(digest)
            return blobs[digest]
        def json_descriptor(desc):
            with descriptor(desc).open('rb') as source:
                return json_bytes(source.read(MAX_JSON + 1))
        index = metadata.get('index.json', {})
        # Single-platform export only; build disables attestations and extra manifests.
        if index.get('schemaVersion') != 2 or len(index.get('manifests', [])) != 1:
            raise ValueError('only one OCI image manifest is supported')
        desc = index['manifests'][0]
        if desc.get('mediaType') != 'application/vnd.oci.image.manifest.v1+json':
            raise ValueError('unsupported OCI manifest')
        manifest = json_descriptor(desc)
        if manifest.get('schemaVersion') != 2 or manifest.get('mediaType') != desc['mediaType']:
            raise ValueError('invalid OCI image manifest')
        cfg_desc = manifest.get('config', {})
        if cfg_desc.get('mediaType') != 'application/vnd.oci.image.config.v1+json':
            raise ValueError('unsupported OCI config')
        cfg = json_descriptor(cfg_desc)
        if cfg.get('os') != 'linux' or cfg.get('architecture') != 'amd64':
            raise ValueError('unsupported image platform')
        if secret_shaped(json.dumps(cfg)):
            raise ValueError('credential in image config or history')
        image_config = cfg.get('config', {})
        for env in image_config.get('Env', []) or []:
            if not isinstance(env, str) or '=' not in env or credential_env(env.split('=', 1)[0]):
                raise ValueError('credential environment variable')
        layers = manifest.get('layers', [])
        diff_ids = cfg.get('rootfs', {}).get('diff_ids', [])
        if not 1 <= len(layers) <= 256 or len(layers) != len(diff_ids):
            raise ValueError('invalid OCI layers')
        installed_manifest = None
        expanded = 0
        for layer, diff_id in zip(layers, diff_ids):
            blob = descriptor(layer)
            media = layer.get('mediaType')
            if media not in {'application/vnd.oci.image.layer.v1.tar', 'application/vnd.oci.image.layer.v1.tar+gzip'}:
                raise ValueError('unsupported OCI layer compression')
            # Bound decompression and verify the uncompressed DiffID before tar parsing.
            unpacked = Path(directory) / 'layer.tar'
            digest = hashlib.sha256()
            opener = gzip.open if media.endswith('+gzip') else open
            with opener(blob, 'rb') as source, unpacked.open('wb') as output:
                while data := source.read(1024 * 1024):
                    expanded += len(data)
                    if expanded > MAX_EXPANDED:
                        raise ValueError('OCI expanded layer size limit')
                    digest.update(data)
                    output.write(data)
            if 'sha256:' + digest.hexdigest() != diff_id:
                raise ValueError('OCI layer DiffID mismatch')
            layer_names = set()
            replacement = None
            deleted = False
            with tarfile.open(unpacked, mode='r|') as archive:
                for item in archive:
                    entries += 1
                    if entries > MAX_ENTRIES:
                        raise ValueError('OCI entry limit')
                    name = safe_path(item.name)
                    if name in layer_names:
                        raise ValueError('duplicate layer member')
                    layer_names.add(name)
                    # Whiteouts affect the final view, never erase scan findings.
                    if name in {'opt/aq/.wh.manifest.json', 'opt/.wh.aq', '.wh.opt',
                                '.wh..wh..opq', 'opt/.wh..wh..opq', 'opt/aq/.wh..wh..opq'}:
                        deleted = True
                    if credential_path(name) and not item.isdir():
                        raise ValueError('credential path detected')
                    if item.issym() or item.islnk():
                        target = posixpath.normpath(posixpath.join(posixpath.dirname(name), item.linkname)) if item.issym() else item.linkname
                        if credential_link_path(name) or credential_link_path(target):
                            raise ValueError('credential link detected')
                    if name in {'opt', 'opt/aq', 'opt/aq/manifest.json'} and not item.isdir():
                        if name != 'opt/aq/manifest.json' or not item.isfile():
                            deleted = True
                    if item.isfile():
                        data = scan_content(archive.extractfile(item), name == 'opt/aq/manifest.json')
                        if data is not None:
                            replacement = validate_bundle(json_bytes(data))
            if deleted:
                installed_manifest = None
            if replacement is not None:
                installed_manifest = replacement
        if used != set(blobs):
            raise ValueError('unreferenced OCI blobs refuse admission')
        if installed_manifest is None:
            raise ValueError('bundle manifest missing from final image')
        return {'image_digest': desc['digest'], 'bundle_manifest': installed_manifest,
                'layers_scanned': len(layers)}


def scan_oci(path):
    """Verify a single-platform OCI archive and scan *every* layer, including deleted files."""
    try:
        return _scan_oci(path)
    except (OSError, EOFError, tarfile.TarError, KeyError, TypeError, AttributeError, RecursionError) as error:
        raise ValueError('invalid OCI image archive') from error


def scan_snapshot(path):
    """Scan captured VM changes and Docker volumes without extracting any member.

    Boat omits unmodified stock-image files; the OCI is scanned separately. This
    scan covers every captured member, including deleted-layer residue stored by
    Docker. No credential-bearing paths or links receive a content exception.
    """
    path = Path(path)
    if path.stat().st_size > MAX_ARCHIVE:
        raise ValueError('snapshot size limit')
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    entries = files = expanded = 0
    names = set()
    try:
        with tarfile.open(path, mode='r|*') as archive:
            for item in archive:
                entries += 1
                expanded += item.size
                if entries > MAX_ENTRIES or expanded > MAX_EXPANDED:
                    raise ValueError('snapshot expanded limits')
                name = safe_path(item.name)
                if name in names:
                    raise ValueError('duplicate snapshot member')
                names.add(name)
                if credential_path(name) and not item.isdir():
                    raise ValueError('credential snapshot path')
                if item.issym() or item.islnk():
                    target = posixpath.normpath(posixpath.join(posixpath.dirname(name), item.linkname)) if item.issym() else item.linkname
                    if credential_link_path(name) or credential_link_path(target):
                        raise ValueError('credential snapshot link')
                    if target.startswith('/'):
                        target = target[1:]
                    safe_path(target)
                elif item.isfile():
                    scan_content(archive.extractfile(item))
                    files += 1
                elif not item.isdir():
                    raise ValueError('unsupported snapshot member')
    except (OSError, EOFError, tarfile.TarError) as error:
        raise ValueError('invalid snapshot archive') from error
    if not files:
        raise ValueError('empty snapshot refuses admission')
    return {'archive_digest': 'sha256:' + digest.hexdigest(), 'files_scanned': files,
            'credential_scan_policy': 'aq-credential-scan-v1'}
