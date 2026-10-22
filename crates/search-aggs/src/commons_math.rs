//! Apache Commons Math 3.6.1's `TDistribution.cumulativeProbability` (what `t_test` reports),
//! ported so p-values match Elasticsearch bit for bit.
//!
//! Commons Math computes through its own `FastMath` rather than the JVM's: `exp`, `log` and
//! `log1p` are table-driven with double-double ("split") arithmetic. The tables come from the
//! same procedures Commons Math generated its literal tables with (`FastMathCalc`: `expint`,
//! `slowexp`, `slowLog`), built once on first use; the `exp` table's entries for `exp(-501)`
//! and below, which Commons Math's shipped literals don't reproduce from those procedures,
//! are embedded as bit patterns. On top: `Gamma` (`logGamma`, `logGamma1p`, `invGamma1pm1`,
//! `gamma`), `Beta.logBeta` (the NSWC algorithm), the continued fraction of
//! `Beta.regularizedBeta` (modified Lentz), and the t-distribution's CDF.
//!
//! Java forbids fused multiply-add contraction and so does Rust, so every expression below
//! rounds exactly as the Java source does; the operation order is kept as written there.

// Constants are written exactly as Commons Math writes them (they parse to the same doubles).
#![allow(clippy::excessive_precision, clippy::unreadable_literal)]

use std::sync::OnceLock;

const HEX_40000000: f64 = 1_073_741_824.0;
const EXP_INT_MAX: usize = 750;

/// `FastMathCalc.FACT`.
const FACT: [f64; 20] = [
    1.0,
    1.0,
    2.0,
    6.0,
    24.0,
    120.0,
    720.0,
    5040.0,
    40320.0,
    362880.0,
    3628800.0,
    39916800.0,
    479001600.0,
    6227020800.0,
    87178291200.0,
    1307674368000.0,
    20922789888000.0,
    355687428096000.0,
    6402373705728000.0,
    121645100408832000.0,
];

/// `FastMathCalc.LN_SPLIT_COEF`.
const LN_SPLIT_COEF: [[f64; 2]; 16] = [
    [2.0, 0.0],
    [0.6666666269302368, 3.9736429850260626E-8],
    [0.3999999761581421, 2.3841857910019882E-8],
    [0.2857142686843872, 1.7029898543501842E-8],
    [0.2222222089767456, 1.3245471311735498E-8],
    [0.1818181574344635, 2.4384203044354907E-8],
    [0.1538461446762085, 9.140260083262505E-9],
    [0.13333332538604736, 9.220590270857665E-9],
    [0.11764700710773468, 1.2393345855018391E-8],
    [0.10526403784751892, 8.251545029714408E-9],
    [0.0952233225107193, 1.2675934823758863E-8],
    [0.08713622391223907, 1.1430250008909141E-8],
    [0.07842259109020233, 2.404307984052299E-9],
    [0.08371849358081818, 1.176342548272881E-8],
    [0.030589580535888672, 1.2958646899018938E-9],
    [0.14982303977012634, 1.225743062930824E-8],
];

const LN_2_A: f64 = 0.693147063255310059;
const LN_2_B: f64 = 1.17304635250823482e-7;
const TWO_POWER_52: f64 = 4503599627370496.0;

/// `FastMath.LN_QUICK_COEF`.
const LN_QUICK_COEF: [[f64; 2]; 9] = [
    [1.0, 5.669184079525E-24],
    [-0.25, -0.25],
    [0.3333333134651184, 1.986821492305628E-8],
    [-0.25, -6.663542893624021E-14],
    [0.19999998807907104, 1.1921056801463227E-8],
    [-0.1666666567325592, -7.800414592973399E-9],
    [0.1428571343421936, 5.650007086920087E-9],
    [-0.12502530217170715, -7.44321345601866E-11],
    [0.11113807559013367, 9.219544613762692E-9],
];

/// `FastMath.LN_HI_PREC_COEF`.
const LN_HI_PREC_COEF: [[f64; 2]; 6] = [
    [1.0, -6.032174644509064E-23],
    [-0.25, -0.25],
    [0.3333333134651184, 1.9868161777724352E-8],
    [-0.2499999701976776, -2.957007209750105E-8],
    [0.19999954104423523, 1.5830993332061267E-10],
    [-0.16624879837036133, -2.6033824355191673E-8],
];

/// `EXP_INT_TABLE_A/B[1..=249]` (`exp(-749)` .. `exp(-501)`) of Commons Math's literal tables.
const EXP_INT_NEG_TAIL: [(u64, u64); 249] = [
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0x0008bfe560000000, 0x80000000021fdcc8),
    (0x0017c8ab40000000, 0x800000001d773655),
    (0x003029ae00000000, 0x80000000772f6aa1),
    (0x0045f7b080000000, 0x80000000a69f80d8),
    (0x005ddb62c0000000, 0x0000000106b3018e),
    (0x00744a3840000000, 0x80000006c6b5e852),
    (0x008b93ad00000000, 0x00000006b36f7ea5),
    (0x00a2bd8c00000000, 0x0000002f6927b7f1),
    (0x00b9788b00000000, 0x8000003ca7bc6917),
    (0x00d14f2b00000000, 0x000000fb9307f0ac),
    (0x00e7869500000000, 0x8000002c118677d7),
    (0x00fff989c0000000, 0x800007a4bc913f0e),
    (0x0115baa640000000, 0x0000085e5781a5da),
    (0x012d886cc0000000, 0x80001cf66a378759),
    (0x014411d740000000, 0x0000c2005a6b68d7),
    (0x015b470cc0000000, 0x000181ca5458dc9e),
    (0x0172897980000000, 0x80003053944e6418),
    (0x018931c4c0000000, 0x0004378ae2427a43),
    (0x01a11f1280000000, 0x8001ba22ad12824d),
    (0x01b7453680000000, 0x801054c4e5bfcc93),
    (0x01cfa0b100000000, 0x0042945ed5eb19e3),
    (0x01e57e45c0000000, 0x0018b6b3a4fca2be),
    (0x01fd365d00000000, 0x007770a03fd8608d),
    (0x0213da1340000000, 0x808a0cc244eedc0e),
    (0x022afb4180000000, 0x0099e7d01d149b49),
    (0x024255f780000000, 0x00c78a679ffe8bec),
    (0x0258ebc340000000, 0x00be8e460eaa3375),
    (0x0270ef7f80000000, 0x00f293e5da4bf7de),
    (0x0287048d80000000, 0x010c2dcb016ef5f9),
    (0x029f48cf80000000, 0x811d9e2f9c058cf5),
    (0x02b5428d00000000, 0x81071d0710d9998a),
    (0x02cce53180000000, 0x010a80de56191947),
    (0x02e3a2ea00000000, 0x012c991ee5a66099),
    (0x02fab048c0000000, 0x017c506ec00461cf),
    (0x03122304c0000000, 0x018d1a825ba96162),
    (0x0328a68440000000, 0x01980acaf97d8049),
    (0x0340c070c0000000, 0x01c6b59ca40eaebf),
    (0x0356c49880000000, 0x81d94ae3de428cb6),
    (0x036ef1e200000000, 0x81ed201ac50fca72),
    (0x0385077a40000000, 0x8204c5f96500347f),
    (0x039c94e780000000, 0x8202cf8c1c399ff4),
    (0x03b36c5a00000000, 0x023621103ee2a2e5),
    (0x03ca662080000000, 0x8217f10fc7e5c7d3),
    (0x03e1f09f80000000, 0x026699e8fa43d040),
    (0x03f86205c0000000, 0x825bace64f60d61d),
    (0x041091e4c0000000, 0x029c3968c81448cc),
    (0x0426855500000000, 0x82a7b82a650941f9),
    (0x043e9be600000000, 0x82b274d99c7c61df),
    (0x0454cd0b80000000, 0x829fa5a39f262ed9),
    (0x046c457c80000000, 0x02bc760c5b861e4d),
    (0x04833661c0000000, 0x02e87686da54f5f0),
    (0x049a1cc640000000, 0x8313ce1fd9ddcd75),
    (0x04b1bec680000000, 0x8339abf7457e6ec7),
    (0x04c81e4580000000, 0x832f40f9ad73b754),
    (0x04e063da40000000, 0x8350658799df7f1d),
    (0x04f646c140000000, 0x8369cf9bfb5f10de),
    (0x050e46d900000000, 0x838cc28d081a8fe4),
    (0x0524933f40000000, 0x83a29891acfff320),
    (0x053bf6ee40000000, 0x838acb165f3ed804),
    (0x055300ff80000000, 0x83d383d1d7b3ae1d),
    (0x0569d437c0000000, 0x83e2ad8126391aa4),
    (0x05818d77c0000000, 0x83ed6ed4a21157cf),
    (0x0597db4180000000, 0x83fdf8414a46d1f0),
    (0x05b0364f80000000, 0x04008488bfeddd24),
    (0x05c608db40000000, 0x044eb484ad41c946),
    (0x05ddf2b840000000, 0x8453e67681c62101),
    (0x05f45a1380000000, 0x84638a3c1ec80abc),
    (0x060ba93a40000000, 0x845a562e583c17cc),
    (0x0622cc3180000000, 0x84a773dad5f3c6bb),
    (0x06398c72c0000000, 0x04a4195c8cf7fcce),
    (0x06515cb200000000, 0x04c958fac382b9d4),
    (0x066798f7c0000000, 0x84e10baae427a84b),
    (0x0680094340000000, 0x05078974d2ee347d),
    (0x0695cba180000000, 0x04ffe15497b6cca0),
    (0x06ad9f8140000000, 0x8511126bc15d0136),
    (0x06c4218680000000, 0x054a4110e4ee1021),
    (0x06db5c5e40000000, 0x855ac83772094996),
    (0x06f297f600000000, 0x057e22c99d57c9da),
    (0x0709457540000000, 0x05822f08692bc181),
    (0x07212c7400000000, 0x85aa442a03c0909a),
    (0x0737576600000000, 0x05b61d6efa5e8f92),
    (0x074fb968c0000000, 0x85bd993b072858b4),
    (0x07658f11c0000000, 0x05e0fda40131c0b8),
    (0x077d4d3180000000, 0x85d0cb3b91748ccc),
    (0x0793e99700000000, 0x861fed011da002ed),
    (0x07ab105780000000, 0x062e40bcd821c8d9),
    (0x07c2644c00000000, 0x8634e2bef62cb2cb),
    (0x07d8ff3d00000000, 0x065b9cb88272ac8e),
    (0x07f0fcbbc0000000, 0x066686acc46c8d52),
    (0x0807168ac0000000, 0x8681ce938f271f57),
    (0x081f614240000000, 0x0692e8284ab8e880),
    (0x0835532a80000000, 0x86bef1a75ca2a4ab),
    (0x084cfbc680000000, 0x86b2bcee3b855775),
    (0x0863b24280000000, 0x06e3142dab3a6443),
    (0x087ac52440000000, 0x06f665858a6ec8dc),
    (0x0892313140000000, 0x071b8b02c5bb1139),
    (0x08a8b9c800000000, 0x06f1e4afbdbdbe27),
    (0x08c0cd8840000000, 0x073194a25ba0a511),
    (0x08d6d66380000000, 0x87286b60eb779b22),
    (0x08ef0a1100000000, 0x876bdc56fbba2333),
    (0x090517e980000000, 0x878b6a287e2a8f22),
    (0x091cab3dc0000000, 0x87938e2cd0dfef45),
    (0x09337b8800000000, 0x078b2a68d435efaf),
    (0x094a7ac200000000, 0x07956398902dbb5c),
    (0x0961fea4c0000000, 0x07a33d172514128e),
    (0x0978751400000000, 0x87f81791d9d56f50),
    (0x09909ed800000000, 0x88121cf17a454e20),
    (0x09a696ee80000000, 0x081b43a6f0ef0720),
    (0x09beb3d1c0000000, 0x880d7eacf265af74),
    (0x09d4dd4d00000000, 0x084a2580e248591a),
    (0x09ec5b94c0000000, 0x88675e1767f272da),
    (0x0a03456580000000, 0x08675c40a9a16223),
    (0x0a1a312e40000000, 0x089e7ad230e4c72a),
    (0x0a31cca480000000, 0x08bbf94c4057dc3b),
    (0x0a48311ec0000000, 0x88b74706b592e47a),
    (0x0a6070a940000000, 0x08dc3f20b235fa0c),
    (0x0a76582a00000000, 0x88ee2106e20a77d9),
    (0x0a8e5e8240000000, 0x88e713d9aed5ee49),
    (0x0aa4a35380000000, 0x092108d4902d18e7),
    (0x0abc0cc900000000, 0x8904bd907b190fda),
    (0x0ad30fd980000000, 0x894845b2ddb74c2d),
    (0x0ae9e86740000000, 0x89647cc7e8d7c825),
    (0x0b019b2f80000000, 0x898a6bf63666252a),
    (0x0b17ede640000000, 0x0995271177638af4),
    (0x0b3042fb00000000, 0x0970675d23373906),
    (0x0b461a13c0000000, 0x89989eb37d587bb6),
    (0x0b5e0a1fc0000000, 0x09b000d289b73c18),
    (0x0b7469fb40000000, 0x89f40cfc9d8cc91f),
    (0x0b8bbed840000000, 0x09ddd9f77f722705),
    (0x0ba2dae240000000, 0x8a247ea570d1c3ed),
    (0x0bb9a06a40000000, 0x8a2c9800a66ddb78),
    (0x0bd16a4380000000, 0x0a5bf9113a71e9bb),
    (0x0be7ab68c0000000, 0x8a333cdaafdcd9ea),
    (0x0c0015cbc0000000, 0x8a8df802570592ae),
    (0x0c15dcaa00000000, 0x0a89f39b8adcda3b),
    (0x0c2db6a7c0000000, 0x0a8bd77332dd959d),
    (0x0c44314200000000, 0x0ace359eac906efa),
    (0x0c5b71c000000000, 0x0adb3ded84084165),
    (0x0c72a67e00000000, 0x0aeeada513517a1e),
    (0x0c89593540000000, 0x8abb410f9b5feb43),
    (0x0ca139dfc0000000, 0x0ae4e1510d047061),
    (0x0cb769a400000000, 0x8b394d691a8a9913),
    (0x0ccfd233c0000000, 0x8b4f19b2004a2603),
    (0x0ce59feb00000000, 0x0b2bceea8bbe4b38),
    (0x0cfd6417c0000000, 0x8b60cf3d456500d4),
    (0x0d13f926c0000000, 0x8b92ef4b58d3b6d2),
    (0x0d2b257e40000000, 0x8ba5d20abf827dd5),
    (0x0d4272ab80000000, 0x0ba9b5d8378a9e52),
    (0x0d5912c640000000, 0x8bd7f4c018e2b8b0),
    (0x0d710a0240000000, 0x0bfbddce8ff62d86),
    (0x0d872895c0000000, 0x0bfe33bce217957b),
    (0x0d9f79c840000000, 0x0c1ed5e0eb852688),
    (0x0db563d4c0000000, 0x0bf0e0647c2100d3),
    (0x0dcd126d00000000, 0x0c41767244a9cd60),
    (0x0de3c1a740000000, 0x8c6d6d6fe214e89b),
    (0x0dfada1000000000, 0x0c7d51025792ce97),
    (0x0e123f6900000000, 0x8c6dd45124a60d69),
    (0x0e28cd1ac0000000, 0x0c8a7962384a2643),
    (0x0e40daaa00000000, 0x8cb3e70ce3f3a922),
    (0x0e56e83c80000000, 0x8cb3ec7f43369b56),
    (0x0e6f2252c0000000, 0x0cd76ed46633edd0),
    (0x0e85286580000000, 0x8cf429e37dabd9c0),
    (0x0e9cc1a540000000, 0x0d171d69e8c67806),
    (0x0eb38ac1c0000000, 0x0d2b14bc21ed7ae6),
    (0x0eca8f73c0000000, 0x8d481d547151aedb),
    (0x0ee20cb500000000, 0x8d6f3d096c948774),
    (0x0ef8883100000000, 0x8d53ba8a9a25933a),
    (0x0f10abd500000000, 0x0d9e4c704185448c),
    (0x0f26a89600000000, 0x8d97a597d0f30e3c),
    (0x0f3ecbd040000000, 0x8d88f59143a3a201),
    (0x0f54ed9b40000000, 0x0dd06932ee8e431f),
    (0x0f6c71be00000000, 0x0de205dd8d518ef2),
    (0x0f83547500000000, 0x0dc7af870e70aeb7),
    (0x0f9a45a680000000, 0x0dec13f5033cc756),
    (0x0fb1da8dc0000000, 0x8e37de225300546b),
    (0x0fc84406c0000000, 0x8e44828e51e2d4f2),
    (0x0fe07d8280000000, 0x8e68ed194a1d7944),
    (0x0ff669a040000000, 0x0e6380296c38d50e),
    (0x100e763e00000000, 0x0e7e259a72851c5b),
    (0x1024b37480000000, 0x0e869ab92982d32b),
    (0x103c22b4c0000000, 0x0eb28063bd31ee47),
    (0x10531ebf00000000, 0x0ed68dd58df1363f),
    (0x1069fca680000000, 0x8edfbb82ffcd7252),
    (0x1081a8f1c0000000, 0x0ef5d3809eb174c9),
    (0x10980099c0000000, 0x0eee92ceeefd0dd4),
    (0x10b04fb080000000, 0x8f39333a70ee38ed),
    (0x10c62b5980000000, 0x0f404f39386140d7),
    (0x10de2199c0000000, 0x8f59a08f961ff61c),
    (0x10f479ef40000000, 0x0f6f7994470632b7),
    (0x110bd48740000000, 0x8f805d6fe0b1c8d7),
    (0x1122e99e80000000, 0x8fa679e42b9b806c),
    (0x1139b47140000000, 0x8fa8175a174a979f),
    (0x115177dfc0000000, 0x0fb787cecdd1eb4e),
    (0x1167bde840000000, 0x8fd4737bb8dfa8b1),
    (0x1180225dc0000000, 0x8ff058b8758ac005),
    (0x1195edc000000000, 0x901e3e266ea2b80b),
    (0x11adcde080000000, 0x9021e5574c9a3ac9),
    (0x11c4410a00000000, 0x90424df6cea5c7ca),
    (0x11db8732c0000000, 0x103cd090889d9f93),
    (0x11f2b51140000000, 0x107bb3d971955c70),
    (0x12096d0480000000, 0x108b5a68ff48fd8a),
    (0x1221475600000000, 0x10aa05beb042b6f8),
    (0x12377bf000000000, 0x909ce93535919028),
    (0x124feb1200000000, 0x90be7483f645f3cd),
    (0x1265b0d140000000, 0x10ed45dc40621ab1),
    (0x127d7b0fc0000000, 0x10f8c3abb5aa4215),
    (0x129408c2c0000000, 0x911c938963a937bd),
    (0x12ab3ab540000000, 0x90f31c60a457737a),
    (0x12c2811640000000, 0x1142e75f7d70d4ce),
];

// ---- FastMathCalc: double-double helpers that build the tables ----

fn split(d: f64) -> [f64; 2] {
    if d < 8e298 && d > -8e298 {
        let a = d * HEX_40000000;
        let s0 = (d + a) - a;
        [s0, d - s0]
    } else {
        let a = d * 9.31322574615478515625E-10;
        let s0 = (d + a - d) * HEX_40000000;
        [s0, d - s0]
    }
}

fn resplit(a: &mut [f64; 2]) {
    let c = a[0] + a[1];
    let d = -(c - a[0] - a[1]);
    if c < 8e298 && c > -8e298 {
        let z = c * HEX_40000000;
        a[0] = (c + z) - z;
        a[1] = c - a[0] + d;
    } else {
        let z = c * 9.31322574615478515625E-10;
        a[0] = (c + z - c) * HEX_40000000;
        a[1] = c - a[0] + d;
    }
}

fn split_mult(a: &[f64; 2], b: &[f64; 2]) -> [f64; 2] {
    let mut ans = [a[0] * b[0], a[0] * b[1] + a[1] * b[0] + a[1] * b[1]];
    resplit(&mut ans);
    ans
}

fn split_add(a: &[f64; 2], b: &[f64; 2]) -> [f64; 2] {
    let mut ans = [a[0] + b[0], a[1] + b[1]];
    resplit(&mut ans);
    ans
}

fn split_reciprocal(input: &[f64; 2]) -> [f64; 2] {
    let b = 1.0 / 4194304.0;
    let a = 1.0 - b;
    let mut inp = *input;
    if inp[0] == 0.0 {
        inp[0] = inp[1];
        inp[1] = 0.0;
    }
    let mut r = [a / inp[0], (b * inp[0] - a * inp[1]) / (inp[0] * inp[0] + inp[0] * inp[1])];
    if r[1].is_nan() {
        r[1] = 0.0;
    }
    resplit(&mut r);
    for _ in 0..2 {
        let mut err = 1.0 - r[0] * inp[0] - r[0] * inp[1] - r[1] * inp[0] - r[1] * inp[1];
        err *= r[0] + r[1];
        r[1] += err;
    }
    r
}

fn quad_mult(a: &[f64; 2], b: &[f64; 2]) -> [f64; 2] {
    let mut xs = split(a[0]);
    let mut ys = split(b[0]);
    let mut zs = split_mult(&xs, &ys);
    let mut r = zs;
    let acc = |r: &mut [f64; 2], zs: [f64; 2]| {
        let tmp = r[0] + zs[0];
        r[1] -= tmp - r[0] - zs[0];
        r[0] = tmp;
        let tmp = r[0] + zs[1];
        r[1] -= tmp - r[0] - zs[1];
        r[0] = tmp;
    };
    ys = split(b[1]);
    zs = split_mult(&xs, &ys);
    acc(&mut r, zs);
    xs = split(a[1]);
    ys = split(b[0]);
    zs = split_mult(&xs, &ys);
    acc(&mut r, zs);
    xs = split(a[1]);
    ys = split(b[1]);
    zs = split_mult(&xs, &ys);
    acc(&mut r, zs);
    r
}

/// `FastMathCalc.expint`: `e^p` in double-double.
fn expint(mut p: u32) -> [f64; 2] {
    let mut xs = [std::f64::consts::E, 1.4456468917292502E-16];
    let mut ys = split(1.0);
    while p > 0 {
        if p & 1 != 0 {
            ys = quad_mult(&ys, &xs);
        }
        xs = quad_mult(&xs, &xs);
        p >>= 1;
    }
    resplit(&mut ys);
    ys
}

/// `FastMathCalc.slowexp`: Taylor series in double-double.
fn slowexp(x: f64) -> [f64; 2] {
    let xs = split(x);
    let mut ys = [0.0, 0.0];
    for i in (0..FACT.len()).rev() {
        ys = split_mult(&xs, &ys);
        let facts = split_reciprocal(&split(FACT[i]));
        ys = split_add(&ys, &facts);
    }
    ys
}

/// `FastMathCalc.slowLog`.
fn slow_log(xi: f64) -> [f64; 2] {
    let mut x = split(xi);
    x[0] += 1.0;
    resplit(&mut x);
    let a = split_reciprocal(&x);
    x[0] -= 2.0;
    resplit(&mut x);
    let x = split_mult(&x, &a);
    let x2 = split_mult(&x, &x);
    let mut y = LN_SPLIT_COEF[LN_SPLIT_COEF.len() - 1];
    for i in (0..LN_SPLIT_COEF.len() - 1).rev() {
        y = split_mult(&y, &x2);
        y = split_add(&y, &LN_SPLIT_COEF[i]);
    }
    split_mult(&y, &x)
}

struct Tables {
    exp_int_a: Vec<f64>,
    exp_int_b: Vec<f64>,
    exp_frac_a: Vec<f64>,
    exp_frac_b: Vec<f64>,
    ln_mant: Vec<[f64; 2]>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut exp_int_a = vec![0.0; 2 * EXP_INT_MAX];
        let mut exp_int_b = vec![0.0; 2 * EXP_INT_MAX];
        for i in 0..EXP_INT_MAX {
            let t = expint(i as u32);
            exp_int_a[EXP_INT_MAX + i] = t[0];
            exp_int_b[EXP_INT_MAX + i] = t[1];
            if i != 0 {
                if EXP_INT_MAX - i <= EXP_INT_NEG_TAIL.len() {
                    let (a, b) = EXP_INT_NEG_TAIL[EXP_INT_MAX - i - 1];
                    exp_int_a[EXP_INT_MAX - i] = f64::from_bits(a);
                    exp_int_b[EXP_INT_MAX - i] = f64::from_bits(b);
                } else {
                    let r = split_reciprocal(&t);
                    exp_int_a[EXP_INT_MAX - i] = r[0];
                    exp_int_b[EXP_INT_MAX - i] = r[1];
                }
            }
        }
        let mut exp_frac_a = Vec::with_capacity(1025);
        let mut exp_frac_b = Vec::with_capacity(1025);
        for i in 0..1025 {
            let t = slowexp(i as f64 / 1024.0);
            exp_frac_a.push(t[0]);
            exp_frac_b.push(t[1]);
        }
        let ln_mant = (0..1024u64).map(|i| slow_log(f64::from_bits((i << 42) | 0x3ff0_0000_0000_0000))).collect();
        Tables { exp_int_a, exp_int_b, exp_frac_a, exp_frac_b, ln_mant }
    })
}

// ---- FastMath ----

/// `FastMath.exp(double)`.
pub fn exp(x: f64) -> f64 {
    exp_inner(x, 0.0)
}

fn exp_inner(x: f64, extra: f64) -> f64 {
    let mut int_val = x as i32;
    if x < 0.0 {
        if x < -746.0 {
            return 0.0;
        }
        if int_val < -709 {
            return exp_inner(x + 40.19140625, extra) / 285040095144011776.0;
        }
        if int_val == -709 {
            return exp_inner(x + 1.494140625, extra) / 4.455505956692756620;
        }
        int_val -= 1;
    } else if int_val > 709 {
        return f64::INFINITY;
    }
    let t = tables();
    let idx = (EXP_INT_MAX as i32 + int_val) as usize;
    let int_a = t.exp_int_a[idx];
    let int_b = t.exp_int_b[idx];
    let int_frac = ((x - int_val as f64) * 1024.0) as i32;
    let frac_a = t.exp_frac_a[int_frac.clamp(0, 1024) as usize];
    let frac_b = t.exp_frac_b[int_frac.clamp(0, 1024) as usize];
    let epsilon = x - (int_val as f64 + int_frac as f64 / 1024.0);
    let mut z = 0.04168701738764507;
    z = z * epsilon + 0.1666666505023083;
    z = z * epsilon + 0.5000000000042687;
    z = z * epsilon + 1.0;
    z = z * epsilon + -3.940510424527919E-20;
    let temp_a = int_a * frac_a;
    let temp_b = int_a * frac_b + int_b * frac_a + int_b * frac_b;
    let temp_c = temp_b + temp_a;
    if temp_c == f64::INFINITY {
        return f64::INFINITY;
    }
    if extra != 0.0 { temp_c * extra * z + temp_c * extra + temp_c * z + temp_b + temp_a } else { temp_c * z + temp_b + temp_a }
}

/// `FastMath.log(double)`.
pub fn log(x: f64) -> f64 {
    log_inner(x, None)
}

fn log_inner(x: f64, hi_prec: Option<&mut [f64; 2]>) -> f64 {
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    let mut bits = x.to_bits() as i64;
    if (bits as u64 & 0x8000_0000_0000_0000 != 0 || x.is_nan()) && x != 0.0 {
        if let Some(h) = hi_prec {
            h[0] = f64::NAN;
        }
        return f64::NAN;
    }
    if x == f64::INFINITY {
        if let Some(h) = hi_prec {
            h[0] = f64::INFINITY;
        }
        return f64::INFINITY;
    }
    let mut exp = (bits >> 52) as i32 - 1023;
    if bits & 0x7ff0_0000_0000_0000 == 0 {
        bits <<= 1;
        while bits & 0x0010_0000_0000_0000 == 0 {
            exp -= 1;
            bits <<= 1;
        }
    }
    if (exp == -1 || exp == 0) && x < 1.01 && x > 0.99 && hi_prec.is_none() {
        let xa0 = x - 1.0;
        let tmp = xa0 * HEX_40000000;
        let aa0 = xa0 + tmp - tmp;
        let ab0 = xa0 - aa0;
        let (xa, xb) = (aa0, ab0);
        let last = LN_QUICK_COEF[LN_QUICK_COEF.len() - 1];
        let (mut ya, mut yb) = (last[0], last[1]);
        for i in (0..LN_QUICK_COEF.len() - 1).rev() {
            let aa = ya * xa;
            let ab = ya * xb + yb * xa + yb * xb;
            let tmp = aa * HEX_40000000;
            ya = aa + tmp - tmp;
            yb = aa - ya + ab;
            let c = LN_QUICK_COEF[i];
            let aa = ya + c[0];
            let ab = yb + c[1];
            let tmp = aa * HEX_40000000;
            ya = aa + tmp - tmp;
            yb = aa - ya + ab;
        }
        let aa = ya * xa;
        let ab = ya * xb + yb * xa + yb * xb;
        let tmp = aa * HEX_40000000;
        ya = aa + tmp - tmp;
        yb = aa - ya + ab;
        return ya + yb;
    }
    let lnm = tables().ln_mant[((bits & 0x000f_fc00_0000_0000) >> 42) as usize];
    let epsilon = (bits & 0x3ff_ffff_ffff) as f64 / (TWO_POWER_52 + (bits & 0x000f_fc00_0000_0000) as f64);
    let (lnza, lnzb);
    if hi_prec.is_some() {
        let tmp = epsilon * HEX_40000000;
        let aa = epsilon + tmp - tmp;
        let ab = epsilon - aa;
        let xa = aa;
        let mut xb = ab;
        let numer = (bits & 0x3ff_ffff_ffff) as f64;
        let denom = TWO_POWER_52 + (bits & 0x000f_fc00_0000_0000) as f64;
        let aa = numer - xa * denom - xb * denom;
        xb += aa / denom;
        let last = LN_HI_PREC_COEF[LN_HI_PREC_COEF.len() - 1];
        let (mut ya, mut yb) = (last[0], last[1]);
        for i in (0..LN_HI_PREC_COEF.len() - 1).rev() {
            let aa = ya * xa;
            let ab = ya * xb + yb * xa + yb * xb;
            let tmp = aa * HEX_40000000;
            ya = aa + tmp - tmp;
            yb = aa - ya + ab;
            let c = LN_HI_PREC_COEF[i];
            let aa = ya + c[0];
            let ab = yb + c[1];
            let tmp = aa * HEX_40000000;
            ya = aa + tmp - tmp;
            yb = aa - ya + ab;
        }
        let aa = ya * xa;
        let ab = ya * xb + yb * xa + yb * xb;
        lnza = aa + ab;
        lnzb = -(lnza - aa - ab);
    } else {
        let mut z = -0.16624882440418567;
        z = z * epsilon + 0.19999954120254515;
        z = z * epsilon + -0.2499999997677497;
        z = z * epsilon + 0.3333333333332802;
        z = z * epsilon + -0.5;
        z = z * epsilon + 1.0;
        z *= epsilon;
        lnza = z;
        lnzb = 0.0;
    }
    let exp = exp as f64;
    let mut a = LN_2_A * exp;
    let mut b = 0.0;
    let mut c = a + lnm[0];
    let mut d = -(c - a - lnm[0]);
    a = c;
    b += d;
    c = a + lnza;
    d = -(c - a - lnza);
    a = c;
    b += d;
    c = a + LN_2_B * exp;
    d = -(c - a - LN_2_B * exp);
    a = c;
    b += d;
    c = a + lnm[1];
    d = -(c - a - lnm[1]);
    a = c;
    b += d;
    c = a + lnzb;
    d = -(c - a - lnzb);
    a = c;
    b += d;
    if let Some(h) = hi_prec {
        h[0] = a;
        h[1] = b;
    }
    a + b
}

/// `FastMath.log1p(double)`.
pub fn log1p(x: f64) -> f64 {
    if x == -1.0 {
        return f64::NEG_INFINITY;
    }
    if x == f64::INFINITY {
        return f64::INFINITY;
    }
    if !(-1e-6..=1e-6).contains(&x) {
        let xpa = 1.0 + x;
        let xpb = -(xpa - 1.0 - x);
        let mut hi = [0.0; 2];
        let lores = log_inner(xpa, Some(&mut hi));
        if lores.is_infinite() {
            return lores;
        }
        let fx1 = xpb / xpa;
        let epsilon = 0.5 * fx1 + 1.0;
        epsilon * fx1 + hi[1] + hi[0]
    } else {
        let y = (x * (1.0 / 3.0) - 0.5) * x + 1.0;
        y * x
    }
}

// ---- Gamma ----

const LANCZOS_G: f64 = 607.0 / 128.0;
const LANCZOS: [f64; 15] = [
    0.99999999999999709182,
    57.156235665862923517,
    -59.597960355475491248,
    14.136097974741747174,
    -0.49191381609762019978,
    0.33994649984811888699e-4,
    0.46523628927048575665e-4,
    -0.98374475304879564677e-4,
    0.15808870322491248884e-3,
    -0.21026444172410488319e-3,
    0.21743961811521264320e-3,
    -0.16431810653676389022e-3,
    0.84418223983852743293e-4,
    -0.26190838401581408670e-4,
    0.36899182659531622704e-5,
];

const A0: f64 = 0.611609510448141581788E-08;
const A1: f64 = 0.624730830116465516210E-08;
const B: [f64; 8] = [
    0.203610414066806987300E+00,
    0.266205348428949217746E-01,
    0.493944979382446875238E-03,
    -0.851419432440314906588E-05,
    -0.643045481779353022248E-05,
    0.992641840672773722196E-06,
    -0.607761895722825260739E-07,
    0.195755836614639731882E-09,
];
const P: [f64; 7] = [
    0.6116095104481415817861E-08,
    0.6871674113067198736152E-08,
    0.6820161668496170657918E-09,
    0.4686843322948848031080E-10,
    0.1572833027710446286995E-11,
    -0.1249441572276366213222E-12,
    0.4343529937408594255178E-14,
];
const Q: [f64; 4] = [0.3056961078365221025009E+00, 0.5464213086042296536016E-01, 0.4956830093825887312020E-02, 0.2692369466186361192876E-03];
const C_NEG: f64 = -0.422784335098467139393487909917598E+00;
const C: [f64; 14] = [
    0.577215664901532860606512090082402E+00,
    -0.655878071520253881077019515145390E+00,
    -0.420026350340952355290039348754298E-01,
    0.166538611382291489501700795102105E+00,
    -0.421977345555443367482083012891874E-01,
    -0.962197152787697356211492167234820E-02,
    0.721894324666309954239501034044657E-02,
    -0.116516759185906511211397108401839E-02,
    -0.215241674114950972815729963053648E-03,
    0.128050282388116186153198626328164E-03,
    -0.201348547807882386556893914210218E-04,
    -0.125049348214267065734535947383309E-05,
    0.113302723198169588237412962033074E-05,
    -0.205633841697760710345015413002057E-06,
];

fn half_log_2_pi() -> f64 {
    static V: OnceLock<f64> = OnceLock::new();
    *V.get_or_init(|| 0.5 * log(2.0 * std::f64::consts::PI))
}

/// `Gamma.lanczos`.
fn lanczos(x: f64) -> f64 {
    let mut sum = 0.0;
    for i in (1..LANCZOS.len()).rev() {
        sum += LANCZOS[i] / (x + i as f64);
    }
    sum + LANCZOS[0]
}

/// `Gamma.invGamma1pm1` (`-0.5 <= x <= 1.5`).
fn inv_gamma1pm1(x: f64) -> f64 {
    let t = if x <= 0.5 { x } else { (x - 0.5) - 0.5 };
    // The tail of the C polynomial shared by both branches, from C12 down to C1.
    let tail = |mut c: f64| {
        for k in (1..=12).rev() {
            c = C[k] + t * c;
        }
        c
    };
    if t < 0.0 {
        let a = A0 + t * A1;
        let mut b = B[7];
        for k in (0..7).rev() {
            b = B[k] + t * b;
        }
        b = 1.0 + t * b;
        let c = C_NEG + t * tail(C[13] + t * (a / b));
        if x > 0.5 { t * c / x } else { x * ((c + 0.5) + 0.5) }
    } else {
        let mut p = P[6];
        for k in (0..6).rev() {
            p = P[k] + t * p;
        }
        let mut q = Q[3];
        for k in (0..3).rev() {
            q = Q[k] + t * q;
        }
        q = 1.0 + t * q;
        let c = C[0] + t * tail(C[13] + (p / q) * t);
        if x > 0.5 { (t / x) * ((c - 0.5) - 0.5) } else { x * c }
    }
}

/// `Gamma.logGamma1p` (`-0.5 <= x <= 1.5`).
fn log_gamma1p(x: f64) -> f64 {
    -log1p(inv_gamma1pm1(x))
}

/// `Gamma.logGamma`.
pub fn log_gamma(x: f64) -> f64 {
    if x.is_nan() || x <= 0.0 {
        f64::NAN
    } else if x < 0.5 {
        log_gamma1p(x) - log(x)
    } else if x <= 2.5 {
        log_gamma1p((x - 0.5) - 0.5)
    } else if x <= 8.0 {
        let n = (x - 1.5).floor() as i32;
        let mut prod = 1.0;
        for i in 1..=n {
            prod *= x - i as f64;
        }
        log_gamma1p(x - (n + 1) as f64) + log(prod)
    } else {
        let sum = lanczos(x);
        let tmp = x + LANCZOS_G + 0.5;
        ((x + 0.5) * log(tmp)) - tmp + half_log_2_pi() + log(sum / x)
    }
}

/// `Gamma.gamma` for `|x| <= 20` (what `Beta.logBeta` needs; larger arguments return NaN
/// rather than going through `FastMath.pow`).
fn gamma(x: f64) -> f64 {
    if x == x.round_ties_even() && x <= 0.0 {
        return f64::NAN;
    }
    if x.abs() > 20.0 {
        return f64::NAN;
    }
    if x >= 1.0 {
        let mut prod = 1.0;
        let mut t = x;
        while t > 2.5 {
            t -= 1.0;
            prod *= t;
        }
        prod / (1.0 + inv_gamma1pm1(t - 1.0))
    } else {
        let mut prod = x;
        let mut t = x;
        while t < -0.5 {
            t += 1.0;
            prod *= t;
        }
        1.0 / (prod * (1.0 + inv_gamma1pm1(t)))
    }
}

// ---- Beta ----

const HALF_LOG_TWO_PI: f64 = 0.9189385332046727;
const DELTA: [f64; DELTA_LEN] = [
    0.833333333333333333333333333333E-01,
    -0.277777777777777777777777752282E-04,
    0.793650793650793650791732130419E-07,
    -0.595238095238095232389839236182E-09,
    0.841750841750832853294451671990E-11,
    -0.191752691751854612334149171243E-12,
    0.641025640510325475730918472625E-14,
    -0.295506514125338232839867823991E-15,
    0.179643716359402238723287696452E-16,
    -0.139228964661627791231203060395E-17,
    0.133802855014020915603275339093E-18,
    -0.154246009867966094273710216533E-19,
    0.197701992980957427278370133333E-20,
    -0.234065664793997056856992426667E-21,
    0.171348014966398575409015466667E-22,
];
const DELTA_LEN: usize = 15;

/// `Beta.logGammaSum` (`1 <= a, b <= 2`).
fn log_gamma_sum(a: f64, b: f64) -> f64 {
    let x = (a - 1.0) + (b - 1.0);
    if x <= 0.5 {
        log_gamma1p(1.0 + x)
    } else if x <= 1.5 {
        log_gamma1p(x) + log1p(x)
    } else {
        log_gamma1p(x - 1.0) + log(x * (1.0 + x))
    }
}

/// `Beta.logGammaMinusLogGammaSum` (`a >= 0`, `b >= 10`).
fn log_gamma_minus_log_gamma_sum(a: f64, b: f64) -> f64 {
    let (d, w) = if a <= b { (b + (a - 0.5), delta_minus_delta_sum(a, b)) } else { (a + (b - 0.5), delta_minus_delta_sum(b, a)) };
    let u = d * log1p(a / b);
    let v = a * (log(b) - 1.0);
    if u <= v { (w - u) - v } else { (w - v) - u }
}

/// `Beta.deltaMinusDeltaSum` (`0 <= a <= b`, `b >= 10`).
fn delta_minus_delta_sum(a: f64, b: f64) -> f64 {
    let h = a / b;
    let p = h / (1.0 + h);
    let q = 1.0 / (1.0 + h);
    let q2 = q * q;
    let mut s = [0.0; DELTA_LEN];
    s[0] = 1.0;
    for i in 1..DELTA_LEN {
        s[i] = 1.0 + (q + q2 * s[i - 1]);
    }
    let sqrt_t = 10.0 / b;
    let t = sqrt_t * sqrt_t;
    let mut w = DELTA[DELTA_LEN - 1] * s[DELTA_LEN - 1];
    for i in (0..DELTA_LEN - 1).rev() {
        w = t * w + DELTA[i] * s[i];
    }
    w * p / b
}

/// `Beta.sumDeltaMinusDeltaSum` (`p, q >= 10`).
fn sum_delta_minus_delta_sum(p: f64, q: f64) -> f64 {
    let a = p.min(q);
    let b = p.max(q);
    let sqrt_t = 10.0 / a;
    let t = sqrt_t * sqrt_t;
    let mut z = DELTA[DELTA_LEN - 1];
    for i in (0..DELTA_LEN - 1).rev() {
        z = t * z + DELTA[i];
    }
    z / a + delta_minus_delta_sum(a, b)
}

/// `Beta.logBeta(p, q)`.
pub fn log_beta(p: f64, q: f64) -> f64 {
    if p.is_nan() || q.is_nan() || p <= 0.0 || q <= 0.0 {
        return f64::NAN;
    }
    let a = p.min(q);
    let b = p.max(q);
    if a >= 10.0 {
        let w = sum_delta_minus_delta_sum(a, b);
        let h = a / b;
        let c = h / (1.0 + h);
        let u = -(a - 0.5) * log(c);
        let v = b * log1p(h);
        if u <= v { (((-0.5 * log(b) + HALF_LOG_TWO_PI) + w) - u) - v } else { (((-0.5 * log(b) + HALF_LOG_TWO_PI) + w) - v) - u }
    } else if a > 2.0 {
        if b > 1000.0 {
            let n = (a - 1.0).floor() as i32;
            let mut prod = 1.0;
            let mut ared = a;
            for _ in 0..n {
                ared -= 1.0;
                prod *= ared / (1.0 + ared / b);
            }
            (log(prod) - n as f64 * log(b)) + (log_gamma(ared) + log_gamma_minus_log_gamma_sum(ared, b))
        } else {
            let mut prod1 = 1.0;
            let mut ared = a;
            while ared > 2.0 {
                ared -= 1.0;
                let h = ared / b;
                prod1 *= h / (1.0 + h);
            }
            if b < 10.0 {
                let mut prod2 = 1.0;
                let mut bred = b;
                while bred > 2.0 {
                    bred -= 1.0;
                    prod2 *= bred / (ared + bred);
                }
                log(prod1) + log(prod2) + (log_gamma(ared) + (log_gamma(bred) - log_gamma_sum(ared, bred)))
            } else {
                log(prod1) + log_gamma(ared) + log_gamma_minus_log_gamma_sum(ared, b)
            }
        }
    } else if a >= 1.0 {
        if b > 2.0 {
            if b < 10.0 {
                let mut prod = 1.0;
                let mut bred = b;
                while bred > 2.0 {
                    bred -= 1.0;
                    prod *= bred / (a + bred);
                }
                log(prod) + (log_gamma(a) + (log_gamma(bred) - log_gamma_sum(a, bred)))
            } else {
                log_gamma(a) + log_gamma_minus_log_gamma_sum(a, b)
            }
        } else {
            log_gamma(a) + log_gamma(b) - log_gamma_sum(a, b)
        }
    } else if b >= 10.0 {
        log_gamma(a) + log_gamma_minus_log_gamma_sum(a, b)
    } else {
        log(gamma(a) * gamma(b) / gamma(a + b))
    }
}

/// `Beta.regularizedBeta(x, a, b)` (epsilon `1e-14`, unbounded iterations); NaN where Commons
/// Math throws (a diverging continued fraction).
pub fn regularized_beta(x: f64, a: f64, b: f64) -> f64 {
    if x.is_nan() || a.is_nan() || b.is_nan() || !(0.0..=1.0).contains(&x) || a <= 0.0 || b <= 0.0 {
        f64::NAN
    } else if x > (a + 1.0) / (2.0 + b + a) && 1.0 - x <= (b + 1.0) / (2.0 + b + a) {
        1.0 - regularized_beta(1.0 - x, b, a)
    } else {
        let e = exp((a * log(x)) + (b * log1p(-x)) - log(a) - log_beta(a, b));
        e * 1.0 / beta_fraction(x, a, b, 1e-14)
    }
}

/// `ContinuedFraction.evaluate` with `Beta.regularizedBeta`'s terms (modified Lentz).
fn beta_fraction(x: f64, a: f64, b: f64, epsilon: f64) -> f64 {
    const SMALL: f64 = 1e-50;
    let is_zero = |v: f64| v.abs() <= SMALL;
    let get_b = |n: u32| -> f64 {
        if n.is_multiple_of(2) {
            let m = n as f64 / 2.0;
            (m * (b - m) * x) / ((a + (2.0 * m) - 1.0) * (a + (2.0 * m)))
        } else {
            let m = (n as f64 - 1.0) / 2.0;
            -((a + m) * (a + b + m) * x) / ((a + (2.0 * m)) * (a + (2.0 * m) + 1.0))
        }
    };
    let mut h_prev = 1.0;
    let mut d_prev = 0.0;
    let mut c_prev = h_prev;
    let mut n: u32 = 1;
    loop {
        let an = 1.0;
        let bn = get_b(n);
        let mut dn = an + bn * d_prev;
        if is_zero(dn) {
            dn = SMALL;
        }
        let mut cn = an + bn / c_prev;
        if is_zero(cn) {
            cn = SMALL;
        }
        dn = 1.0 / dn;
        let delta = cn * dn;
        let hn = h_prev * delta;
        if !hn.is_finite() {
            return f64::NAN;
        }
        if (delta - 1.0).abs() < epsilon {
            return hn;
        }
        d_prev = dn;
        c_prev = cn;
        h_prev = hn;
        n += 1;
        if n == i32::MAX as u32 {
            return f64::NAN;
        }
    }
}

/// `new TDistribution(degreesOfFreedom).cumulativeProbability(x)`.
pub fn t_cdf(degrees_of_freedom: f64, x: f64) -> f64 {
    if x == 0.0 {
        return 0.5;
    }
    let t = regularized_beta(degrees_of_freedom / (degrees_of_freedom + (x * x)), 0.5 * degrees_of_freedom, 0.5);
    if x < 0.0 { 0.5 * t } else { 1.0 - 0.5 * t }
}
