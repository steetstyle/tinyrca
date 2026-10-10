# INCIDENTS

Bulgular, ölçümü ve kök neden analiziyle birlikte kayıt altına alınır. Her kayıt
bir problemi tanımlar; düzeltmesi, kayıtta listelenen sırayla yapılır ve her
adımın sorgusu ile çıktısı kayda işlenir.

Şablon: [aşağıdaki `INC-0001`](#inc-0001).

---

<a id="inc-0001"></a>
## INC-0001 — Log deposuna yazılan veri kayboluyor, "depo boş" deniyor

**Durum:** düzeltildi (4/4 adım, aşağıda günlük)
**Tarih:** 2026-10-10
**Kapsam:** tinylink, tiny-rca
**Etkilenen araçlar:** `tinylink logs-ingest`, `tiny_rca.search_logs`,
`tiny_rca.capabilities`

---

### Problem

ClickHouse'tan 29.347 exception logu OTLP'ye çevirip ingest edildi. Dosya 42 MB
yazıldı, `logs-ingest` "Ingested 29347 record(s)" dedi. Buna rağmen
`search_logs source=store` şunu döndürdü:

```json
{ "store_records": 0, "total_matched": 0,
  "note": "The log store is empty. Ingest OTLP payloads with `tinylink logs-ingest
           <path>` or the tinylink ingest_logs tool before searching, or use
           source=database for the deployment's own logs." }
```

**Kullanıcıya göre veri yok, gerçekte yanlış dosyaya yazılmıştı.**

---

### Yeniden üretim

```bash
# 1. Index'in yanına türetilen varsayılan yol bu:
#    tinylink/src/mcp/tools.rs  default_log_store_path()
#      index_path.set_extension("logs.db")   ->  unicpeak-analytics.logs.db

# 2. Yanlış dosyaya ingest et
tinylink logs-ingest payload.json --store ~/.cache/tinylink/incident-logs.db
#   -> Ingested 29347 record(s) into .../incident-logs.db

# 3. Sorgula
search_logs(source="store", min_severity="Error")
#   -> store_records: 0, "The log store is empty."
```

---

### Ölçülen kanıt

| gözlem | değer |
|---|---|
| Ingest edilen kayıt | 29.347 |
| `search_logs source=store` → `store_records` | **0** |
| `capabilities` çıktısında log deposu yolu | **yok** |
| `search_logs` "depo boş" notunda depo yolu | **yok** |
| Yazılan dosya | `~/.cache/tinylink/incident-logs.db` |
| Okunan dosya | `~/.cache/tinylink/unicpeak-analytics.logs.db` |

Doğru dosyaya ingest edildiğinde aynı sorgu döndü:

```json
{ "store_records": 29347, "total_matched": 29347 }
```

---

### Kök neden

`default_log_store_path()` log deposunu index yolundan **türetiyor**:

```rust
// tinylink/src/mcp/tools.rs:668
pub fn default_log_store_path(index_path: &Path) -> PathBuf {
    if let Ok(from_env) = std::env::var("TINYLINK_LOG_STORE") {
        if !from_env.is_empty() { return PathBuf::from(from_env); }
    }
    let mut path = index_path.to_path_buf();
    path.set_extension("logs.db");
    path
}
```

`logs-ingest --store` ile yazılan yol ile MCP sunucusunun okuduğu yol arasında
**hiçbir doğrulama yok**. İkisi ayrı yere baktığında:

1. `logs-ingest` başarılı görünüyor — dosya gerçekten yazıldı.
2. `search_logs` başarısız görünüyor — çünkü başka bir dosyayı okuyor.
3. Aradaki mesaj **"depo boş"** diyor, yani *"yanlış yere yazdın"* değil.

**Daha derin kök neden (ölçümle bulundu, ilk yazımda eksikti):**

CLI ile sunucunun varsayılanları **zaten farklı**, `--store` hiç verilmeden
kullanılan durumda:

| yer | varsayılan | kaynak |
|---|---|---|
| `tinylink logs-ingest <payload>` | `./tinylink.logs.db` (çalışma dizini) | `tinylink/src/main.rs:348` `resolve_log_store` |
| `tinylink serve --index X` | `X.logs.db` (index'in kardeşi) | `tinylink/src/mcp/tools.rs:668` `default_log_store_path` |

Yani belgede "verilmezse" denilen durumda iki komut **asla aynı dosyaya
bakmıyor**. Bu, tuzağın `TINYLINK_LOG_STORE` bilinmediğinden değil, varsayılanların
çelişmesinden doğuyor. Ben de tam olarak bu yüzden düştüm: `--store` vermedim,
yazdım, okutamadım.

** güven : yüksek.** Her iki varsayılan da kodda okundu ve davranış ölçüldü.

**ikincil neden:** `capabilities` log deposunun yolunu veya kayıt sayısını
raporlamıyor. Aracı "hangi depoya yazmalıyım" sorusuna cevap veremediği için
belirsizlik ortada kalıyor.

---

### Etki alanı

Yanlış depoya yazan herkes aynı şekilde ilerleyecek: ingest başarılı, sorgu boş,
ve mesaj kullanıcıyı **ikinci kez ingest etmeye** yönlendiriyor — oysa veri
duruyor, sadece yanlış dosyada.

Bu sessiz bir hatadır: ne `error` ne uyarı üretir, sadece yanlış cevap verir.

---

### Düzeltme planı

Sıra önemli — 1 görünürlük, 2 doğrulama.

| # | Değişiklik | Nerede | Nasıl doğrulanır |
|---|---|---|---|
| 1 | `capabilities` log deposu yolunu ve kayıt sayısını bildirsin | tiny-rca `capabilities` | `capabilities` çıktısında `log_store` alanı |
| 2 | `search_logs source=store` "depo boş" dediğinde **okuduğu yolu** söylesin | tiny-rca `search_logs` | yanlış depoda sorgu → notta gerçek yol görünür |
| 3 | `logs-ingest` **iki varsayılanın çeliştiğini** söylesin ve eşleştirme yolunu göstersin | tinylink `logs-ingest` | `--store` vermeden çalıştır → uyarı |
| 3b | CLI varsayılanı ile sunucu varsayılanı **tek kurala** bağlansın | tinylink `main.rs` + `mcp/tools.rs` | ikisi aynı yola düşüyor mu |
| 4 | `praxis_begin` boş depoyu "kanıt yok" diye okumasın | tinypraxis | log sayısı 0 iken not düşsün |

---

### Düzeltme günlüğü

Sıra plandaki sırayla. Her adımda önce değişiklik, sonra ölçüm.

---

#### Adım 1/4 — `capabilities` ve `search_logs` log deposunun yolunu bildirsin ✅

**Sorgu (önce):**

```
$ grep -rn "log_store\|logs.db\|store_records" tiny-rca/src/tools.rs
1163:    if out["store_records"] == json!(0) {
```

**Sonuç:** `capabilities` log deposundan hiç bahsetmiyor. Tek eşleşme, boş depoya
verilen not. Depo yolu hiçbir yerde görünmüyor.

**Sorgu (tinylink tarafı — yol buradan geliyor):**

```
$ sed -n '666,678p' tinylink/src/mcp/tools.rs
pub fn default_log_store_path(index_path: &Path) -> PathBuf {
    if let Ok(from_env) = std::env::var("TINYLINK_LOG_STORE") {
        if !from_env.is_empty() { return PathBuf::from(from_env); }
    }
    let mut path = index_path.to_path_buf();
    path.set_extension("logs.db");
    path
}
```

**Sonuç:** yol index'ten türetiliyor. Değişiklik iki depoda:

| dosya | değişiklik |
|---|---|
| `tinylink/src/mcp/tools.rs` | `search_logs` yanıtına `log_store: {path, records, derived_from}` |
| `tinylink/src/mcp/tools.rs` | depo boşsa not **yolu da** içeriyor |
| `tinylink/src/mcp/mod.rs` | handler `self.log_store_path`'i geçiyor |
| `tiny-rca/src/tools.rs` | `capabilities.log_store` — tek `search_logs` sorgusuyla |
| `tiny-rca/src/tools.rs` | `search_logs` boş-depo notu okuduğu yolu söylüyor |

**Sorgu (düzeltme sonrası):**

```
$ capabilities
```

**Sonuç:**

```json
{ "known": true,
  "path": "/home/roy/.cache/tinylink/unicpeak-analytics.logs.db",
  "records": 29347,
  "derived_from": "the index path, unless TINYLINK_LOG_STORE overrides it" }
```

**Sorgu (hatanın yeniden üretimi — başka bir index, loglar burada değil):**

```
$ tiny-rca serve --index /tmp/opencode/bos-index/unicpeak-analytics.db
$ search_logs(source="store", limit=1)
```

**Sonuç:**

```
store_records : 0
note: The log store at /tmp/opencode/bos-index/unicpeak-analytics.logs.db holds no
      records. Ingest with `tinylink logs-ingest <path>` ... writing to a different
      file will not show up here, because the store path is derived from the index
      path unless TINYLINK_LOG_STORE says otherwise.
```

**Kabul ölçütü:** yanlış dosya artık kendi adını söylüyor. Kayıp veri değil,
yanlış yer — ve mesaj ikisini ayırt ediyor.

**Testler:** tinylink 135 lib testi, tiny-rca 28 lib testi; yeni 3 test. Not
üretimi saf fonksiyona çıkarıldı ki sunucu kurmadan test edilebilsin.

---

#### Adım 3/4 — `logs-ingest` varsayılan çelişkisini söylesin ✅

**Sorgu (tespit):**

```
$ grep -n "TINYLINK_LOG_STORE|default_log_store_path" tinylink/src/main.rs
348:        None => match std::env::var("TINYLINK_LOG_STORE") {
```

**Sonuç:** CLI `None` için `tinylink.logs.db` (çalışma dizini) kullanıyor; sunucu
ise `default_log_store_path()` ile index'in kardeşini kullanıyor. `--store`
verilmeden kullanılan durumda **ikisi asla aynı dosyaya bakmıyor**. Kaydın kök
neden bölümü buna göre derinleştirildi.

**Değişiklik:** `logs-ingest` yazdıktan sonra, kullanıcı bir yol belirtmediyse,
sunucunun ne okuyacağını söylüyor ve tek yer belirleme yolunu veriyor.

**Sorgu (düzeltme sonrası, `--store` verilmeden):**

```
$ tinylink logs-ingest /tmp/opencode/exception-logs-otlp.json
```

**Sonuç:**

```
note: wrote tinylink.logs.db. A server started with `tinylink serve --index <path>`
      reads <path>.logs.db instead -- beside its index, not beside the working
      directory -- so it will report an empty store. Name one place and set it for
      both: export TINYLINK_LOG_STORE=tinylink.logs.db
Ingested 29347 record(s) from 1 payload(s) into tinylink.logs.db
  distinct traces:  29347
```

**Sorgu (`--store` ile doğru yola):** uyarı çıkmıyor, yalnızca sonuç.

---

#### Adım 3b — bu sırada **yeni bir hata** çıktı: ingest yinelenebilir değil

Düzeltmeyi doğrularken fark edildi. **Ayrı kayıt: [INC-0002](#inc-0002).**

Kısa özet: aynı payload iki kez ingest edildi → **58.694** kayıt, ve araç iki kez
de "Ingested 29347 record(s)" dedi.

---

#### Adım 4/4 — `praxis_begin` "0 log"un hangisi olduğunu söylesin ✅

**Sorgu (tespit):**

```
$ grep -n "store_records|logs.len()|log_failures" tinypraxis/src/tools.rs
149:    let (logs, log_failures) = if args.include_logs {
182:    let logs_considered = logs.len();
```

**Sonuç:** `store_records` hiç geçmiyor. `logs_considered: 0` üç farklı bulguyu
aynı cevapla veriyor: depo hiç doldurulmamış, arama başarısız olmuş, ya da istekler
gerçekten log yazmamış.

**Değişiklik:** `store_records` `collect_logs`'tan geçirildi ve üç durum ayrı ayrı
söylendi. `explain_no_logs()` saf fonksiyon — dört durumun hepsi sunucu kurmadan
test ediliyor.

**Sorgu (boş depo):**

```
$ tinypraxis serve --rca ... --rca-arg=--index /tmp/opencode/bos-index/...
$ praxis_begin(trace=real_trace.json, include_logs=true, log_source=store)
```

**Sonuç:**

```
logs_considered: 0
log_source_note : The log store holds 0 records, so `logs_considered: 0` means
                  nothing was ever ingested rather than that these requests wrote
                  no logs. Ingest an OTLP payload, or use log_source=database.
```

**Sorgu (dolu depo, ama trace'e ait log yok):**

```
$ tinypraxis serve --rca ... --rca-arg=--index ~/.cache/tinylink/unicpeak-analytics.db
$ praxis_begin(trace=real_trace.json, include_logs=true, log_source=store)
```

**Sonuç:**

```
logs_considered: 0
log_source_note : The store holds 29347 record(s); none belong to these traces.
                  logs_considered: 0 is a real absence, not an unread store.
```

**Kabul ölçütü:** aynı sayı, iki farklı ve doğru anlam. Biri "ingest et" der, diğeri
"yokluğun gerçek, devam et" der. Üçüncü durum (arka uç sayıyı bildirmiyor) ayrıca
söyleniyor.

---

**Planın tamamı.**

| # | adım | durum |
|---|---|---|
| 1 | `capabilities` + `search_logs` depo yolunu bildirsin | ✅ |
| 2 | boş-depo notu okuduğu yolu söylesin | ✅ |
| 3 | `logs-ingest` varsayılan çelişkisini söylesin | ✅ |
| 4 | `praxis_begin` "0 log"un hangisi olduğunu söylesin | ✅ |
| 3b | ingest yinelenebilirliği → [INC-0002](#inc-0002) | ✅ |

---

### Bu kayıttan çıkan, ayrı problemler

Bunlar aynı seansta ölçüldü ama ayrı kök nedenleri var; ayrı kayıt olmaları gerekir.

| # | Problem | Kanıt |
|---|---|---|
| A | `search_logs source=database` boş sonucu JSON zorunluluğuna takılıp hataya dönüşüyordu | `endpoint_health` "No rows matched" → "returned non-JSON" — **düzeltildi**, `eee0502` |
| B | `praxis_begin` başarısız log aramasını "log yok" diye raporluyordu | `log_source=not_a_real_store` → `logs_considered: 0`, hata yok — **düzeltildi** |
| C | El yazımı OTLP dönüşümünde iki sessiz tuzak var | `timeUnixNano` string: log kabul, trace reddediyor. `level=4` → `Severity::Trace`, `min_severity=Error` 29.347 kaydı bulamıyor |
| D | Deployment'tan trace/log çıkarma köprüsü yok | elle yazıldı, araç yok |
| E | Incident bulma giriş noktası yok | 2.000.012 trace'in tümü tek span, tek servis; hiçbir araç "hatalı trace'leri ver" demiyor |

---

### Doğrulanmamış kalanlar

- **`Npgsql.PostgresException` loglarının trace'e bağlanmaması** → çözüldü, bak
  [INC-0003](#inc-0003). Üretici bu 50.000 kaydın id'sini 9 milyon kaydırmış, yani
  span'la eşleşmeleri tasarım gereği imkânsız. Kayıtlar 2026-10-10'da silindi.
- **Çalışan servislerin neden enstrümente olmadığı** (`Agency.API.csproj` yalnızca
  `Rougamo.Fody` referanslı) ayrı bir konu; canlı log toplama imkânını etkiliyor.
- **`TINYLINK_LOG_STORE` ortam değişkeni** ayrıntılı incelenmedi; MCP sunucusu bu
  değeri gördüğü hâlde `capabilities` bunu bildirmiyor.

---

<a id="inc-0002"></a>
## INC-0002 — Aynı payload tekrar ingest edilince depo ikiye katlanıyor

**Durum:** düzeltildi
**Tarih:** 2026-10-10
**Kapsam:** tinylink
**Etkilenen araçlar:** `tinylink logs-ingest`, `tinylink ingest_logs`

---

### Problem

`logs` tablosunda içerik bazlı tekil kısıt yok; `id` otomatik artıyor. Aynı OTLP
payload'ı iki kez ingest etmek her kaydı ikinci kez yazıyor. Araç da bunu
"kaç kayıt eklendi" diye bildiriyor.

---

### Ölçüm

```
$ tinylink logs-ingest payload.json --store X.logs.db     # 1.
  Ingested 29347 record(s) from 1 payload(s) into X.logs.db

$ tinylink logs-ingest payload.json --store X.logs.db     # 2. aynı dosya
  Ingested 29347 record(s) from 1 payload(s) into X.logs.db

$ sqlite3 X.logs.db "SELECT count(*) FROM logs"
  58694
```

**29.347 + 29.347 = 58.694.** Her sayım bundan sonra yanlış: `store_records`,
`total_matched`, `distinct traces`.

---

### Kök neden

`CREATE TABLE logs` üç alanı da saklıyor ama hiçbirini benzersiz kılmıyor. `INSERT`
düz `INSERT INTO`; aynı satır iki kez ekleniyor. `DefaultHasher` da kullanılamaz —
`fingerprint` kalıcı bir sütun, `DefaultHasher`'ın sürümler arası garantisi yok.

** güven : yüksek.** Şema okundu, davranış ölçüldü.

---

### Düzeltme

| # | Değişiklik | Nerede |
|---|---|---|
| 1 | `fingerprint TEXT NOT NULL` + `UNIQUE INDEX` | `log_store.rs` şema |
| 2 | `INSERT OR IGNORE`, `changed == 0` → `report.duplicates += 1` | `insert_batch` |
| 3 | `blake3` ile parça-uzunluklu karma | `fingerprint()` |
| 4 | `records_already_present` alanı, `records_inserted`'a **katlanmıyor** | `ingest_logs_json` |
| 5 | `merge()` `duplicates`'ı topluyor | `merge` |
| 6 | "no log records in payload" yalnız `inserted == 0 && duplicates == 0` ise | `insert_payload` |
| 7 | CLI `note:` satırı | `main.rs` |

**`blake3` zaten bağımlılıktı** — yeni bağımlılık eklenmedi.

---

### Düzeltme günlüğü

**Sorgu (aynı payload iki kez, yeni binary):**

```
$ tinylink logs-ingest payload.json --store Y.logs.db   # 1.
  Ingested 29347 record(s)
$ tinylink logs-ingest payload.json --store Y.logs.db   # 2.
  note: 29347 record(s) were already in the store and were not added again
        (records_inserted: 0).
  Ingested 0 record(s)
$ SELECT count(*) FROM logs   -> 29347
$ SELECT count(DISTINCT fingerprint) FROM logs -> 29347
```

**Sonuç:** depo 29.347'de kaldı, ikinci koşuda 0 eklendi ve **sebebi** yazıldı.

**Sorgu (4 katmanlı sayı kaybı — `merge` düzeltilene kadar not yoktu):**

```
$ tinylink logs-ingest payload.json --store Y.logs.db   # 3.
  Ingested 0 record(s)              # ne "already present" ne de "no log records"
```

**Sonuç:** `insert_batch` sayıyı doğru dolduruyordu, `merge()` onu düşürüyordu.
Ayrıca "no log records in payload" mesajı yanlış tetikleniyordu: payload boş
değil, kayıtlar zaten vardı. İkisi de düzeltildi ve `merge_keeps_the_duplicate_count`
testiyle sabitlendi.

**Sorgu (şema geçişi v1 → v2):**

```
$ sqlite3 ~/.cache/tinylink/unicpeak-analytics.logs.db "SELECT value FROM meta..."
  1        (58.694 kayıt)
$ tinylink logs-search --store aynı
  No matching logs. Store holds 0 record(s).
  sema surumu: 2   kayit: 0
```

**Sonuç:** v1 deposu açılışta **boşaltılıyor**. Bu kasıtlı: repo'nun kendi kuralı
"bayat yerleşim hiçbir şeyden iyidir, yanlış okumaktansa at" diyor. Tahmini
fingerprint uydurmak, dedup'un sessizce yanlış olması demekti. Depo türetilmiş
veriden yeniden üretilebilir; payload elimizdeydi:

```
$ tinylink logs-ingest payload.json --store ...
  Ingested 29347 record(s)
$ tinylink logs-ingest payload.json --store ...   # tekrar
  Ingested 0 record(s)   (29347 zaten vardı)
```

**Maliyet:** `SCHEMA_VERSION = 2` yüzünden mevcut tüm depolar bir kez boşalır ve
yeniden ingest gerektirir. `~/.cache` altındaki tek depo etkilendi ve geri
yüklendi.

**Testler:** tinylink 139 lib testi (`--features mcp-http`), 121 (`--lib`).
Yeni 5 test: aynı payload iki kez, farklı gövde, farklı trace, `merge` sayacı,
depo yolunun bildirilmesi.

---

### Bu kayıttan çıkan, ayrı problemler

| # | Problem | Kanıt |
|---|---|---|
| E | Incident bulma giriş noktası yok | 2.000.012 trace'in tümü tek span, tek servis |
| F | El yazımı OTLP dönüşümünde iki sessiz tuzak | `timeUnixNano` string: log kabul, trace reddediyor · `level=4` → `Severity::Trace`, `min_severity=Error` hiçbirini bulmuyor |

---

### Doğrulanmamış kalanlar

- **`blake3` çakışma olasılığı** ölçülmedi. 64 bit kısaltma olduğu için pratikte
  sıfır kabul edildi, ama kanıtlanmadı. Tam 256 bit sütun olarak saklanabilirdi;
  16 bayt yerine 64 bayt seçildi.
- **Aynı içerik, iki ayrı gerçek olay** olduğunda tekilleştirme onları birleştirir.
  `observed_unix_nano` ve `span_id` fingerprint'e girdiği için aynı istek içinde
  çift kayıt olmadıkça risk düşük; ölçülmedi.
- **`Npgsql.PostgresException`** çözüldü → [INC-0003](#inc-0003).
---

<a id="inc-0003"></a>
## INC-0003 — Sentetik veri: köken doğrulaması ve temizlik

**Durum:** düzeltildi (log tablosu), kapsam bilinçli olarak daraltıldı
**Tarih:** 2026-10-10
**Kapsam:** ClickHouse `telemetry` veritabanı

---

### Problem

`inc-0001` incelenirken "bu veri sentetik" denildi — ama o oturumda doğrulanmamıştı.
Dayanak, veritabanındaki kelimenin kendisiydi (`synthetic failure for index
measurement`). **Yeterli değil:** bir veritabanına `"synthetic"` yazan biri elbette
sentetik yazar.

---

### Yeniden üretim (üç bağımsız kanıt)

**Kanıt 1 — üretici betik diskte, verinin üretildiği gün.**

```
$ find / -name "gen_volume*"
/tmp/opencode/gen_volume.py          13.900 bayt   2026-10-08 23:18

ClickHouse max timestamp                            2026-10-08 19:51
```

**Kanıt 2 — betiğin metinleri veritabanıyla birebir aynı.**

| `gen_volume.py` | veritabanında ölçülen |
|---|---|
| `'Npgsql.PostgresException', 'rare failure', '   at Rare.Handler() in /srv/Rare.cs:line 7'` | aynen |
| `concat('rare occurrence 4242 for token ', number)` | `rare occurrence 4242 for token 16333` |
| `'synthetic failure for index measurement'` | aynen |
| `'/api/rare'`, `'/api/member/'` | aynen |

**Kanıt 3 — aritmetik, id'leri birebir üretiyor.** En güçlüsü bu: metin eşleşmesi
kopyalanabilir, sayı eşleşmesi kopyalanamaz.

Üreticinin formülü, ClickHouse'ta çalıştırıldı:

```
SELECT number, lower(hex(reinterpretAsFixedString(toUInt128(number + 9000000 + 1))))
FROM numbers(3) FORMAT TSVWithNames

sayi  uretilen
0      41548900000000000000000000000000
1      42548900000000000000000000000000
```

`0x00895441` = **9.000.001**. Formül gerçek.

---

### Kök neden (INC-0001'deki açık madde): Npgsql logları neden trace'e bağlanmıyor

**Sorgu:**

```
SELECT exception_type, count(), uniqExact(trace_id),
       countIf(trace_id='') FROM telemetry.log
WHERE exception_type IS NOT NULL GROUP BY 1
```

**Sonuç:**

| exception_type | adet | trace_id boş | farklı trace_id |
|---|---|---|---|
| `Npgsql.PostgresException` | 50.000 | 0 | 50.000 |
| `System.InvalidOperationException` | 29.347 | 0 | 29.347 |

**Sonuç:** id'ler boş değil — **yazılmamış**.

**Sorgu (kesisim):**

```
SELECT count() FROM telemetry.trace t INNER JOIN
  (SELECT DISTINCT trace_id FROM telemetry.log
   WHERE exception_type='Npgsql.PostgresException') l ON t.trace_id=l.trace_id
→ 0
```

**Kök neden — üreticinin kaydırması:**

| küme | formül | aralık |
|---|---|---|
| span'lar (2.000.000) | `number + 1` | 1 … 2.000.000 |
| loglar (500.000) | `number + 1` | 1 … 500.000 ← span aralığının **içinde** |
| Npgsql logları (50.000) | `number + 9.000.001` | 9.000.001 … 9.050.000 ← **dışında** |

Logların `trace_id`'si span aralığının içinde olduğu için 29.347'si eşleşiyor;
Npgsql'ınki 9 milyon kaydırıldığı için **hiçbiri eşleşemiyor**.

**Bu bir veri kalitesi kazası değil, üreticinin bilinçli bir tercihi:** onları
izlenebilir olmaktan çıkarmış.

** güven : yüksek.** Üç bağımsız kanıt, aritmetik dâhil.

---

### Temizlik

**Sorgu (kapsam ölçümü — her işaretin kaç satıra dokunduğu):**

```
SELECT
  countIf(exception_type='Npgsql.PostgresException')     →  50.000
  countIf(exception_message='rare failure')             →  50.000
  countIf(position(exception_stacktrace,'Rare.Handler')>0) → 50.000
  countIf(position(message,'rare occurrence')>0)        →  50.000
  countIf(endpoint='/api/rare')                         →  50.000
  countIf(endpoint LIKE '/api/member/%')                → 500.000   ← ana partinin TAMAMI
  countIf(exception_message='synthetic failure...')     →  29.347
  count()                                                → 550.006
```

**Ölçüm iki şeyi ortaya çıkardı, ikisi de karar öncesi bildirildi:**

1. `/api/member/` yalnızca istisna loglarını değil, **ana log partisinin 500.000
   satırının tamamını** işaretliyor.
2. `trace` tablosundaki **87.671 hatalı span'ın hepsi** sentetik işaretli.

**Karar (kullanıcı):** yalnızca istisna logları, `/api/member/` hariç.
`trace` tablosuna dokunulmadı.

**Sorgu (WHERE'in tam olarak ne sileceğini doğrulama):**

```
SELECT count() FROM telemetry.log WHERE
  exception_type = 'Npgsql.PostgresException'
  OR exception_message = 'rare failure'
  OR position(exception_stacktrace,'Rare.Handler') > 0
  OR position(message,'rare occurrence') > 0
  OR endpoint = '/api/rare'
  OR exception_message = 'synthetic failure for index measurement'
→ 79.347
```

Bu, `exception_type IS NOT NULL` ile ölçülen sayıyla **birebir aynı** — yani
predicate fazlasını değil, tam olarak istisna loglarını kapsıyor.

**Sorgu (yazma izni doğrulama, geri alınamaz işlem öncesi):**

```
SELECT command, is_done FROM system.mutations
→ DELETE WHERE (kind_id = 9) ... is_done = 1     (önceki mutasyonlar çalışmış)
```

**Sorgu (uygulama):**

```
ALTER TABLE telemetry.log DELETE WHERE <yukarıdaki predicate>
SETTINGS mutations_sync = 2
→ hata yok
```

**Sonuç (doğrulama):**

```
kalan_log  kalan_istisna  kalan_api_rare  kalan_rare_occurrence  kalan_sentetik
470.659    0              0               0                      0
```

**Sorgu (kalanın ne olduğu):**

```
normal_log (/api/member/*)  470.653
işletim logu (endpoint NULL)      6   ← Telemetry.Mcp.Server'ın kendi kaydı, sentetik DEĞİL
```

**Sorgu (trace tablosu dokunulmadı mı):**

```
span_toplam  hatali_span
2.000.012    87.671
```

**Değişmedi** — karar gereği.

---

#### Temizlik, ikinci tur — `/api/member/`

Kapsam genişletildi: ana log partisi de silinecek.

**Sorgu (silinecek):**

```
SELECT countIf(endpoint LIKE '/api/member/%')    → 470.653
       count()                                    → 470.659
       kalacak                                    →      6
       istisna olanlar içinde                     →      0
```

**Sorgu (silineceklerin ne olduğu):**

```
message:           synthetic log line 342976
formatted_message: Handler 8 synthetic log line 342976
properties:        {"TenantId":"tenant-1"}
```

Üreticinin çıktısı — hepsi sentetik.

**Sorgu (kalacak 6 satır gerçek mi):**

`endpoint NOT LIKE '/api/member/%'` **boş döndü**, çünkü `endpoint IS NULL` olan
satırlarda `NOT LIKE` NULL verir ve üç-değerli mantıkta elenir. `endpoint IS NULL OR
...` ile tekrar soruldu: 6 satır, hepsi şu:

```
Telemetry filter refreshed: mode={Mode}, rootsOnly={RootsOnly}, traceFull=…
```

**Kaynağı bulundu** — üretici değil, gerçek kod:

```
$ grep -rn "Telemetry filter refreshed" --include=*.cs .
./Infrastructure.Telemetry.Core/TelemetryFilter.cs:390
```

`{Mode}` gibi şablon yer tutucularının görünmesi de bunu doğruluyor: .NET exporter
gövdeyi işlenmemiş şablon olarak yolluyor, bu da tinylink'in dokümanında yazılı.

**Sorgu (sentetik işaret kalmış mı):**

```
sentetik_kelime  rare_kelime  formatted_sentetik  toplam
0                0            0                  6
```

**Sorgu (uygulama):**

```
ALTER TABLE telemetry.log DELETE WHERE endpoint LIKE '/api/member/%'
SETTINGS mutations_sync = 2
→ hata yok
```

**Sonuç (doğrulama):**

```
kalan_log  kalan_member  kalan_rare  kalan_istisna  kalan_sentetik
6          0             0           0              0
```

`telemetry.log` artık tamamen gerçek: 6 satır, hepsi telemetri sunucusunun kendi
işletim kaydı.

---

---

#### Temizlik, üçüncü tur — altı tablo

**Sorgu (kapsam ölçümü, üreticinin yazdığı yerler):**

```
$ grep -nE "INSERT INTO telemetry\." gen_volume.py
122:  telemetry.trace      181: telemetry.log       221: telemetry.baggage
251:  telemetry.trace_path 278: telemetry.trace_path_step  298: telemetry.log
```

`path`, `path_step`, `metric` yazılmamış — **View** oldukları için ölçümde çıktı:

```
name              engine
path              View          → SELECT * FROM telemetry.trace_path
path_step         View          → SELECT * FROM telemetry.trace_path_step
trace_path        AggregatingMergeTree
trace_path_step   ReplacingMergeTree
```

**Sorgu (silinecek satırlar):**

```
tablo            silinecek    toplam
trace            2.000.000    2.000.012
trace_path          50.000       50.000
path                50.000       50.000
path_step          400.000      400.000
```

`trace` için 12 satır **korundu**: `/api/x/` işareti taşımayan tek 12 span. Bunlar
`POST http://localhost:8123/` — telemetri sunucusunun kendi ClickHouse çağrıları,
zamanları kalan log satırlarıyla birebir aynı (`19:51:59`, `19:46:59`).

**Sorgu (path_step'in sahte olduğunun bağımsız kanıtı):**

```
farkli dosya yolu: 1.500   →  /srv/Service0…6/Handler0…499.cs
$ ls -d unicpeak-analytics-api/*/srv   →  yok
```

**Sorgu (uygulama ve doğrulama):**

```
ALTER TABLE telemetry.trace           DELETE WHERE position(attributes,'/api/x/')>0 …
                                        → kalan: 12
ALTER TABLE telemetry.trace_path      DELETE WHERE position(scope_name,'Scope.')=1   → 0
ALTER TABLE telemetry.trace_path_step DELETE WHERE position(file_path,'/srv/Service')=1 → 0
```

`path` ve `path_step` mutasyon hatası verdi — **View**'lar mutasyon desteklemiyor.
Altındaki tablolar boşalınca view'lar da boşaldı, ayrı işlem gerekmedi.

**Son durum:**

```
log                6      ← 6 gerçek işletim logu kaldı
trace             12      ← 12 gerçek işletim span'i kaldı
metric          2.470      ← silinmedi, aşağıda
baggage       4.500.000    ← silinmedi, aşağıda
path                 0
path_step            0
trace_path           0
trace_path_step      0
trace_path_new       0
```

**Sorgu (son tarama):**

```
log_sentetik  trace_sentetik  baggage_sentetik  metric_toplam
0             0               2.250.000         2.470
```

### Silme sonrası bu veritabanının durumu

| tablo | durum |
|---|---|
| `telemetry.log` | **6** — telemetri sunucusunun kendi işletim logları |
| `telemetry.trace` | **12** — sunucunun kendi ClickHouse çağrıları |
| `telemetry.metric` | 2.470 — **silinmedi**, kaynağı kanıtlanmadı |
| `telemetry.baggage` | 4.500.000 — **silinmedi**, kullanıcı listesinde yoktu |
| `telemetry.path` / `path_step` / `trace_path` / `trace_path_step` | **0** |

**Bu veritabanıyla istisna araştırması artık yapılamaz.** Log tablosunda istisna
kalmadı; hatalı span'lar duruyor ama eşleşen logları yok — yani
`search_logs source=database` bir hata döndüğünde görececek kayıt bulamaz.

---

### Doğrulanmamış kalanlar

- **`baggage`, `path`, `path_step`, `trace_path` tabloları da sentetik** (aynı
  betikten çıktı) ama kullanıcı listelemediği için silinmedi. Sırasıyla
  4.500.000 / 50.000 / 400.000 / 50.000 satır.
- **`trace` tablosundaki 87.671 hatalı span** sentetik işaretli ve silinmedi —
  bilinçli, karar gereği.
- **Genetik isimlendirme**: `Npgsql.PostgresException` bir .NET/PostgreSQL sınıfı
  adı, `/srv/Rare.cs` Linux yolu, `at Rare.Handler() in /srv/Rare.cs:line 7` C#
  stack izimi. `/srv/` altında C# kaynağı olamaz — bu da tek başına sentetik
  olduğunun göstergesi, ama kanıt değil.
- **Çalışan servisler hâlâ enstrümente değil.** Bunun sebebi araştırılmadı;
  ölçülen şu: `Agency.API.csproj` yalnızca `Rougamo.Fody` referanslı,
  `OpenTelemetry` yalnızca `Infrastructure.Telemetry.Benchmarks` içinde,
  4317/4318 dinleyen yok.
---

<a id="inc-0004"></a>
## INC-0004 — `capabilities` telemetry sunucusunu "bağlı" diyor, her çağrı başarısız

**Durum:** kök neden bulundu, düzeltme uygulanmadı (aşağıda)
**Tarih:** 2026-10-10
**Kapsam:** tiny-rca, unicpeak-analytics-api (çalıştırma ortamı)
**Etkilenen araçlar:** `tiny_rca.capabilities`, `tiny_rca.telemetry_call`

---

### Problem

`capabilities` telemetry sunucusunu 12 araçla bağlı gösteriyor. Her
`telemetry_call` başarısız oluyor:

```json
{ "server": "telemetry", "connected": true,
  "tools": ["list_telemetry_scopes", "path_diff", "error_paths", "search_logs",
            "explain_trace", "new_paths", "capture_coverage", "search_spans",
            "find_by_baggage", "endpoint_health", "path_profile", "trace_logs"] }
```

```text
telemetry_call list_telemetry_scopes -> Error: telemetry server call to
  'list_telemetry_scopes' failed: Transport closed
telemetry_call endpoint_health      -> Error: telemetry server call to
  'endpoint_health' failed: Transport closed
```

**Kullanıcıya göre telemeti bağlı görünüyor, gerçekte veritabanı erişilemez.**

> **Düzeltme (aynı gün, ölçüm sonrası).** Aşağıdaki 4. maddenin ilk hâlî
> yanlıştı: "süreç ölmüş" değil. Taze başlatılan bir sunucuda .NET alt süreci
> **ayakta** ve `tools/list`'e yanıt veriyor, yani `connected: true` doğru. Ölçülen
> şu:
>
> ```text
> telemetry_call list_telemetry_scopes
>   -> ERROR: telemetry server reported:
>      An error occurred invoking 'list_telemetry_scopes'.
> ```
>
> Yani hata **yutulmuyor** — `call_for_text` `is_error`'u doğru okuyor
> (`src/backends.rs:237`). İlk ölçümüm bunu "Transport closed" gördü ve boş
> string sandım; aslında hata `result` değil `error` alanında geliyormuş ve benim
> probe'um onu okumuyordu. **İki ayrı hata vardı** ve ikisi de gerçek:
>
> - 2 saat 10 dakika çalışan bir tiny-rca örneğinde .NET alt süreci **gerçekten
>   ölmüş** (`Transport closed`). Bu ayrı bir durum ve `capabilities` onu
>   `connected: true` diye bildiriyordu.
> - Taze başlatılan örnekte süreç yaşıyor, MCP çalışıyor, ama ClickHouse
>   kapalı olduğu için **her veri sorgusu** hata dönüyor.
>
> Gerçek kusur ikincisi ve `capabilities`'ın kapsamı dışında: **"bağlı" demek
> "veritabanı erişilebilir" demek değil.** Aşağıdaki 4. maddenin düzeltilmiş
> hâli aşağıda.**

---

### Yeniden üretim

```bash
# 1. Rapor ne diyor?
#    MCP: tiny_rca.capabilities  ->  "server": "telemetry", "connected": true, 12 arac

# 2. Ayni sunucuya ilk gercek cagri:
#    MCP: tiny_rca.telemetry_call(tool="list_telemetry_scopes")  -> "Transport closed"
```

---

### Kök neden zinciri

Ölçüm, dıştan içe:

**1. `.env` dosyası yok.**

```bash
$ ls -la .env*
(bos)
$ git check-ignore -v .env
.gitignore:142:.env    .env
```

Doğru bir karar — sırlar depoda durmamalı — ama dosya hiç oluşturulmamış.

**2. Telemetry sunucusu bu yüzden açılışta ölüyor.**

```bash
$ dotnet Telemetry.Mcp.Server/bin/Debug/net10.0/Telemetry.Mcp.Server.dll
  Configuration applied from appsettings.json + appsettings.Development.json (11 keys).
  Unhandled exception. System.Exception: APPLICATION_POSTGRESQL_DB_HOST not
  defined on .env file
     at Shared.Identity.Extensions.SharedApplicationExtension
        .$Rougamo_AddAuthorizationClient(IServiceCollection services)
        in Shared.Identity/Extensions/SharedIdentityExtension.cs:line 54
     at Program.$Rougamo_<Main>$(String[] args) in
        Telemetry.Mcp.Server/Program.cs:line 105
```

Beklenen 5 anahtar kodda geçiyor: `APPLICATION_POSTGRESQL_DB_HOST`,
`_PORT`, `_NAME`, `_USER`, `_PASSWORD`. Kod toplam **78** ortam değişkeni
okuyor.

**3. API servisleri de çalışmıyor.** Tüm süreç listesi:

```bash
$ ps aux | grep [d]otnet
(yalnızca tiny-rca ve tinypraxis MCP sunucuları; hiçbir uygulama süreci yok)

$ ss -ltn
  8123 yok   (ClickHouse)
  4317 yok   (OTLP gRPC)
  4318 yok   (OTLP HTTP)
  127.0.0.1:11434  (Ollama — tek açık olan)

$ curl -s -o /dev/null -w "%{http_code}" "http://localhost:8123/?query=SELECT%201"
000
```

Postgres konteynerleri **ayakta** (`test_postgres_app/logging/analytics/billing`,
4'er dakikadır `healthy`), yani veritabanı tarafı hazır, uygulama tarafı değil.

**4. Ve asıl kusur: `capabilities` bunu bilmiyor.**

`src/tools.rs:675` — `connected`, sunucu nesnesi var mı diye yazılıyor:

```rust
match &backends.telemetry {
    Some(link) => servers.push(json!({
        "server": link.name(),
        "connected": true,      // <-- bir cagri yapilmadi
```

`Backends::connect` (`src/backends.rs:108`) gerçek bir el sıkışma yapıyor ve
araç listesini o an alıyor. Sonra .NET süreci ölüyor. `capabilities` o anki
durumu dondurup **bir daha sormuyor**.

Bu, INC-0004'teki ikinci sınıf hata: araç "bağlı" der, kullanıcı çağırır,
çağrı ölü bir sürece gider, ve bu çökme `capabilities` çıktısında **hiç**
görünmez.

---

### Etki

- INC-0003'ün açık kalan maddesi ("çalışan servisler enstrümente değil")
  çözülemez: çalışan servis yok.
- `path_diff`, `capture_coverage`, `find_by_baggage` — runtime yol kanıtı
  üreten araçların hepsi erişilemez.
- `tiny_praxis` telemeti gerektiren adımlarda aynı boşluğa düşer.

---

### Düzeltme sırası (4. madde düzeltildi)

1. ~~`capabilities` bağlantıyı doğrulanmış olarak raporlamalı.~~ **Yapıldı.**
   `McpLink::probe()` artık gerçek bir `tools/list` turu atıyor ve `capabilities`
   `connected`'ı o çağrının sonucuna bağlıyor; ayrıca `configured` ve
   `tools_at_connect_time` alanları eklendi. Ölü alt süreç artık `connected: true`
   görünmüyor.
   - İlk deneme `RunningService::is_closed()` idi ve **çalışmadı**: süreç öldükten
     sonra da `false` döndü. Servis kendi transport'unun kapandığını görmüyor, bu
     yüzden tuttuğu bir bayrak transport kaybını bildiremez.
   - Ölü alt süreç senaryosu gerçek: 2 saat 10 dakika çalışan örnekte `Transport
     closed` ölçüldü. Test `/bin/true` ile kuruluyor, çünkü sınanan şey mock'un
     yalan söylemesi değil, çocuğun ölünce `is_closed()`'in dönmesi.
2. **`.env` üretilmeli — ama telemeti sunucusu için gerekli değil.**
   `.env` eksikliği yalnızca yetkilendirme yolunu (`Shared.Identity`) kırıyor;
   `TELEMETRY_MCP_TRANSPORT=stdio` verildiğinde sunucu **sorunsuz açılıyor**
   (ölçüldü). Yani `.env` uygulama servislerini çalıştırmak için gerekli,
   telemeti okumak için değil.
3. **ClickHouse ayağa kaldırılmalı.** Şu anki asıl engel bu: sunucu ayakta, MCP
   çalışıyor, her veri sorgusu hata dönüyor. `curl localhost:8123` → `000`.
4. **`capabilities`'a veritabanı erişilebilirliği de eklenmeli.** Sırf süreç
   canlı diye "bağlı" demek, bu ortamda yanlış olumlu veriyor. En azından
   `telemetry_call` hatası "database unreachable" ayrımını taşımalı; şu anki
   mesaj (`An error occurred invoking 'list_telemetry_scopes'.`) ne ClickHouse'ın
   kapalı olduğunu ne de başka ne söylüyor. Bu madde 3'ten sonra ölçülebilir.
