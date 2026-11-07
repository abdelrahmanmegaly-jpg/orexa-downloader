// Interface languages. English is the source text; every other language is a
// dictionary from the English wording to its translation, so a missing entry
// simply shows English instead of breaking anything.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    En,
    Ar,
}

impl Lang {
    pub const ALL: [Lang; 2] = [Lang::En, Lang::Ar];

    /// Each language is named in its own script so people can always find theirs.
    pub fn native_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Ar => "العربية",
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ar => "ar",
        }
    }

    pub fn from_code(c: &str) -> Lang {
        match c {
            "ar" => Lang::Ar,
            _ => Lang::En,
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn set(l: Lang) {
    CURRENT.store(if l == Lang::Ar { 1 } else { 0 }, Ordering::Relaxed);
}

pub fn get() -> Lang {
    if CURRENT.load(Ordering::Relaxed) == 1 {
        Lang::Ar
    } else {
        Lang::En
    }
}

static MISSING: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Texts shown in a non-English language that have no translation yet
/// (collected so they can be found while testing).
#[allow(dead_code)]
pub fn missing() -> Vec<String> {
    let g = MISSING.lock().unwrap();
    let mut v: Vec<String> = g.as_ref().map(|s| s.iter().cloned().collect()).unwrap_or_default();
    v.sort();
    v
}

fn note_missing(s: &str) {
    // Development aid: OREXA_LOG_MISSING=<file> collects texts without a translation.
    let Some(path) = std::env::var_os("OREXA_LOG_MISSING") else { return };
    if !s.chars().any(|c| c.is_ascii_alphabetic()) {
        return;
    }
    let mut g = MISSING.lock().unwrap();
    if g.get_or_insert_with(HashSet::new).insert(s.to_string()) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{}", s.replace(char::from(10), " / "));
        }
    }
}

/// The translation of `s` in the current language, if there is one.
pub fn translate(s: &str) -> Option<String> {
    match get() {
        Lang::En => None,
        Lang::Ar => {
            let r = ar_exact(s).map(|x| x.to_string()).or_else(|| template(s, AR_TEMPLATES));
            if r.is_none() {
                note_missing(s);
            }
            r
        }
    }
}

fn exact_only(s: &str) -> Option<String> {
    match get() {
        Lang::En => None,
        Lang::Ar => ar_exact(s).map(|x| x.to_string()),
    }
}

/// Match `s` against templates such as "{} files" and build the translated
/// text. Pieces captured by `{}` are translated too when they are known words.
fn template(s: &str, table: &[(&str, &str)]) -> Option<String> {
    template_at(s, table, 0)
}

fn template_at(s: &str, table: &[(&str, &str)], depth: u8) -> Option<String> {
    // Several patterns can fit ("{} file" and "{} peers, {} sharing the whole
    // file"); the one with the most fixed words is the right one.
    let mut best: Option<(usize, String)> = None;
    for (en, tr) in table {
        let literal_len: usize = en.split("{}").map(|p| p.len()).sum();
        if best.as_ref().map_or(false, |(l, _)| *l >= literal_len) {
            continue;
        }
        if let Some(out) = fill_template(s, en, tr, table, depth) {
            best = Some((literal_len, out));
        }
    }
    best.map(|(_, t)| t)
}

fn fill_template(s: &str, en: &str, tr: &str, table: &[(&str, &str)], depth: u8) -> Option<String> {
    {
        let parts: Vec<&str> = en.split("{}").collect();
        if parts.len() < 2 || !s.starts_with(parts[0]) || !s.ends_with(parts[parts.len() - 1]) {
            return None;
        }
        let mut rest = &s[parts[0].len()..];
        // Short patterns that start with a value ("{} files") are for numbers,
        // so they must not catch ordinary names such as "my files".
        if parts[0].is_empty() && parts[parts.len() - 1].len() < 10 && !rest.starts_with(|c: char| c.is_ascii_digit()) {
            return None;
        }
        let mut caps: Vec<String> = Vec::new();
        for (i, lit) in parts.iter().enumerate().skip(1) {
            let last = i == parts.len() - 1;
            let at = if last {
                if rest.len() < lit.len() {
                    return None;
                }
                rest.len() - lit.len()
            } else if lit.is_empty() {
                return None;
            } else {
                rest.find(lit)?
            };
            let cap = &rest[..at];
            caps.push(exact_only(cap).or_else(|| if depth == 0 { template_at(cap, table, 1) } else { None }).unwrap_or_else(|| cap.to_string()));
            rest = &rest[at + lit.len()..];
        }
        // Fill {1}, {2}... in the translation.
        let mut out = tr.to_string();
        for (i, c) in caps.iter().enumerate().rev() {
            out = out.replace(&format!("{{{}}}", i + 1), c);
        }
        Some(out)
    }
}

const AR_TEMPLATES: &[(&str, &str)] = &[
    ("{} selected", "{1} محدد"),
    ("Remove {} downloads?", "إزالة {1} تنزيلات؟"),
    ("Going to sleep in {} s", "سيدخل الجهاز وضع السكون بعد {1} ث"),
    ("Failed: {}", "فشل: {1}"),
    ("Continued with one connection from {}", "استؤنف باتصال واحد من {1}"),
    ("Started with {} connections, the file is split into {} parts", "بدأ بـ {1} اتصالات، وقُسّم الملف إلى {2} جزءًا"),
    ("Continued with {} connections, {}% was already saved", "استؤنف بـ {1} اتصالات، وكان {2}% محفوظًا"),
    ("Part {} was interrupted, trying again ({} of {})", "انقطع الجزء {1}، جارٍ إعادة المحاولة {2} من {3}"),
    ("{} h {} min", "{1} س {2} د"),
    ("{} min {} s", "{1} د {2} ث"),
    ("{} s", "{1} ث"),
    ("{} left", "متبقي {1}"),
    ("{} files", "{1} ملفات"),
    ("{} file", "{1} ملف"),
    ("Sharing, up {}", "مشاركة، رفع {1}"),
    ("{} sent so far", "أُرسل {1} حتى الآن"),
    ("About {}", "حوالي {1}"),
    ("About {}", "حوالي {1}"),
    ("Version {}", "الإصدار {1}"),
    ("Setup failed: {}", "فشل الإعداد: {1}"),
    ("Server answered {}", "ردّ الخادم برمز {1}"),
    ("{} peers, {} sharing the whole file", "{1} مصدر، {2} منهم يشاركون الملف كاملًا"),
    ("{}  (ratio {})", "{1}  النسبة {2}"),
    ("{} of {}", "{1} من {2}"),
    ("{}, reading its files...", "{1}، جارٍ قراءة ملفاته..."),
    ("{}, files not read yet", "{1}، لم تُقرأ الملفات بعد"),
    ("{}, size shows once it starts", "{1}، سيظهر الحجم بعد البدء"),
    ("{}, size unknown", "{1}، الحجم غير معروف"),
    ("{}, checking size...", "{1}، جارٍ فحص الحجم..."),
    ("Size: {}   Saved in: {}", "الحجم: {1}   مكان الحفظ: {2}"),
    ("Type {} in the address bar and press Enter.", "اكتب {1} في شريط العنوان ثم اضغط Enter."),
    ("{} will leave the list. The file on your disk is not deleted.", "سيُحذف {1} من القائمة. لن يُحذف الملف من جهازك."),
    ("About {}", "حوالي {1}"),
    ("{} of {} chosen, {}", "تم اختيار {1} من {2}، {3}"),
    (
        "This link is already in your list ({}: {}). A new copy is saved under a new name.",
        "هذا الرابط موجود في قائمتك بالفعل: {2} ({1}). ستُحفظ نسخة جديدة باسم مختلف.",
    ),
    ("Not enough free space on {}: it needs {} and only {} is free.", "لا توجد مساحة كافية على {1}: المطلوب {2} والمتاح {3} فقط."),
    ("Could not start the torrent engine: {}", "تعذّر تشغيل محرك التورنت: {1}"),
];

fn ar_exact(s: &str) -> Option<&'static str> {
    Some(match s {
        // Sidebar and categories
        "A fast, simple download manager. Save files, web videos, YouTube and torrents, with one click from your browser." => {
            "مدير تنزيلات سريع وبسيط. احفظ الملفات ومقاطع الويب وYouTube والتورنت بنقرة واحدة من متصفحك."
        }
        "Checking..." => "جارٍ الفحص...",
        "Size unknown" => "الحجم غير معروف",
        "Remove from list" => "إزالة من القائمة",
        "Server stopped responding" => "توقف الخادم عن الاستجابة",
        "The download stopped before the end. Try again." => "توقف التنزيل قبل اكتماله. حاول مجددًا.",
        "Could not open the file" => "تعذّر فتح الملف",
        "The connection kept failing. Try again." => "استمر فشل الاتصال. حاول مجددًا.",
        "The download did not match its checksum" => "لم يطابق التنزيل رمز التحقق الخاص به",
        "That is not a torrent file" => "هذا ليس ملف تورنت",
        "Nobody answered yet. It can still start; the files show once it connects." => "لم يرد أحد بعد. يمكنه البدء مع ذلك، وستظهر الملفات عند الاتصال.",
        "Could not read the torrent" => "تعذّرت قراءة التورنت",
        "Download details" => "تفاصيل التنزيل",
        "Direct link" => "رابط مباشر",
        "Video page" => "صفحة فيديو",
        "Type" => "النوع",
        "Time left" => "الوقت المتبقي",
        "Connections" => "الاتصالات",
        "Added" => "تاريخ الإضافة",
        "Link" => "الرابط",
        "Speed over time" => "السرعة عبر الوقت",
        "Activity" => "النشاط",
        "Collecting..." => "جارٍ الجمع...",
        "Shows while it downloads" => "يظهر أثناء التنزيل",
        "Resumed" => "تم الاستئناف",
        "Stopped, waiting" => "توقف، في الانتظار",
        "Started with one connection" => "بدأ باتصال واحد",
        "Welcome to Orexa Downloader" => "أهلًا بك في Orexa Downloader",
        "Add the browser extension so downloads, videos and magnet links open here with one click." => {
            "أضف إضافة المتصفح لتفتح التنزيلات والفيديوهات وروابط magnet هنا بنقرة واحدة."
        }
        "Show me how" => "أرني الطريقة",
        "Maybe later" => "ربما لاحقًا",
        "You can find it again in Settings." => "ستجدها مرة أخرى في الإعدادات.",
        "Clear" => "إلغاء التحديد",
        "They will leave the list. The files on your disk are not deleted." => "ستُزال من القائمة. لن تُحذف الملفات من جهازك.",
        "All downloads are finished" => "اكتملت كل التنزيلات",
        "Sleep now" => "نوم الآن",
        "When all downloads finish" => "عند اكتمال كل التنزيلات",
        "Choose what Orexa does once nothing is left to download." => "اختر ما يفعله Orexa عندما لا يتبقى شيء للتنزيل.",
        "Do nothing" => "لا تفعل شيئًا",
        "Open the folder" => "افتح المجلد",
        "Sleep" => "سكون",
        "Copy diagnostics" => "نسخ معلومات التشخيص",
        "Copied" => "تم النسخ",
        "Copies your version, settings and recent errors, without any links or file names, to paste into a bug report." => {
            "ينسخ إصدارك وإعداداتك وآخر الأخطاء دون أي روابط أو أسماء ملفات، لتلصقها في بلاغ عن مشكلة."
        }
        "Report a problem" => "الإبلاغ عن مشكلة",
        "Keyboard shortcuts" => "اختصارات لوحة المفاتيح",
        "Add a link" => "إضافة رابط",
        "Add the link on the clipboard" => "إضافة الرابط المنسوخ",
        "Search" => "بحث",
        "Select all" => "تحديد الكل",
        "Pause or resume the selected downloads" => "إيقاف التنزيلات المحددة أو استئنافها",
        "Open the selected download" => "فتح التنزيل المحدد",
        "Remove the selected downloads" => "إزالة التنزيلات المحددة",
        "Select several downloads" => "تحديد عدة تنزيلات",
        "See details" => "عرض التفاصيل",
        "Hide details" => "إخفاء التفاصيل",
        "File size" => "حجم الملف",
        "Downloaded" => "تم تنزيل",
        "Library" => "المكتبة",
        "Categories" => "الفئات",
        "Settings" => "الإعدادات",
        "All downloads" => "كل التنزيلات",
        "Downloading" => "قيد التنزيل",
        "Completed" => "مكتملة",
        "Unfinished" => "غير مكتملة",
        "Compressed" => "ملفات مضغوطة",
        "Documents" => "مستندات",
        "Music" => "موسيقى",
        "Programs" => "برامج",
        "Video" => "فيديو",
        "Videos" => "فيديو",
        "Torrents" => "تورنت",
        "Torrent" => "تورنت",
        "Other" => "أخرى",
        "Language" => "اللغة",
        // Toolbar
        "Search downloads" => "ابحث في التنزيلات",
        "Add URL" => "إضافة رابط",
        "Pause all" => "إيقاف الكل مؤقتًا",
        "Restore" => "استعادة",
        "Maximize" => "تكبير",
        "Minimize" => "تصغير",
        "Close to the tray (downloads keep running)" => "إغلاق إلى الشريط (التنزيلات مستمرة)",
        "How to add the extension?" => "كيف أضيف الإضافة؟",
        "How to add the Chrome extension?" => "كيف أضيف إضافة Chrome؟",
        "How to add the Chrome extension" => "كيف أضيف إضافة Chrome",
        // Empty states
        "No downloads yet" => "لا توجد تنزيلات بعد",
        "Paste a link to save your first file. Orexa Downloader keeps it organized and picks up where it left off if your connection drops." => {
            "الصق رابطًا لحفظ أول ملف لك. يحافظ Orexa Downloader على ترتيب ملفاتك ويكمل من حيث توقف إذا انقطع الاتصال."
        }
        "Tip: you can also start from the Add URL button at any time." => "نصيحة: يمكنك أيضًا البدء من زر إضافة رابط في أي وقت.",
        "Nothing here yet" => "لا يوجد شيء هنا بعد",
        "Downloads in this view will show up here." => "ستظهر التنزيلات الخاصة بهذا العرض هنا.",
        "No downloads match your search." => "لا توجد تنزيلات تطابق بحثك.",
        "Today" => "اليوم",
        "Earlier" => "سابقًا",
        "Paused, queued and failed" => "متوقفة ومنتظرة وفاشلة",
        "Current speed" => "السرعة الحالية",
        "None" => "لا شيء",
        "Longest time left" => "أطول وقت متبقٍ",
        "Unknown size" => "الحجم غير معروف",
        "Starting" => "جارٍ البدء",
        "Paused" => "متوقف مؤقتًا",
        "Queued" => "في الانتظار",
        "Failed" => "فشل",
        "Resume any time" => "يمكنك الاستئناف في أي وقت",
        "Waiting for a free slot" => "في انتظار مكان شاغر",
        "Something went wrong" => "حدث خطأ ما",
        "Could not write the file" => "تعذّرت كتابة الملف",
        "Resume" => "استئناف",
        "Pause" => "إيقاف مؤقت",
        "Stop" => "إيقاف",
        "Start now" => "ابدأ الآن",
        "Open" => "فتح",
        "Open file" => "فتح الملف",
        "Open folder" => "فتح المجلد",
        "Show in folder" => "إظهار في المجلد",
        "Delete" => "حذف",
        "Remove" => "إزالة",
        "Keep" => "إبقاء",
        "Details" => "التفاصيل",
        "info" => "معلومات",
        "Copy" => "نسخ",
        "Copy link" => "نسخ الرابط",
        "Retry" => "إعادة المحاولة",
        "Torrent details" => "تفاصيل التورنت",
        // Settings
        "General" => "عام",
        "Changes apply right away." => "تُطبَّق التغييرات فورًا.",
        "Default save folder" => "مجلد الحفظ الافتراضي",
        "New downloads go here." => "تُحفظ التنزيلات الجديدة هنا.",
        "Change" => "تغيير",
        "Sort into folders by type" => "ترتيب الملفات في مجلدات حسب النوع",
        "Save new downloads in Videos, Music, Documents and so on, inside the folder above." => {
            "احفظ التنزيلات الجديدة في مجلدات الفيديو والموسيقى والمستندات وغيرها داخل المجلد أعلاه."
        }
        "Downloads at the same time" => "عدد التنزيلات في وقت واحد",
        "Waiting downloads start when a slot is free." => "تبدأ التنزيلات المنتظرة عندما يتوفر مكان شاغر.",
        "Fewer" => "أقل",
        "More" => "أكثر",
        "Keep sharing finished torrents" => "متابعة مشاركة التورنت بعد اكتماله",
        "Keep uploading a torrent after it finishes, until you stop it. Helps others; uses upload speed." => {
            "استمر في رفع التورنت بعد اكتماله حتى توقفه. يساعد الآخرين لكنه يستهلك سرعة الرفع."
        }
        "Start with Windows" => "التشغيل مع Windows",
        "Open Orexa in the tray when Windows starts." => "افتح Orexa في شريط النظام عند تشغيل Windows.",
        "Open magnet links and .torrent files" => "فتح روابط magnet وملفات .torrent",
        "Orexa opens them when you click one in any program." => "يفتحها Orexa عند النقر عليها في أي برنامج.",
        "Default apps" => "التطبيقات الافتراضية",
        "If another app opens them, choose Orexa in Windows Settings" => "إذا فتحها تطبيق آخر، اختر Orexa من إعدادات Windows",
        "Notifications" => "الإشعارات",
        "Show a Windows notification when a download finishes." => "أظهر إشعار Windows عند اكتمال التنزيل.",
        "Chrome extension" => "إضافة Chrome",
        "Add it to Chrome, Brave or Edge." => "أضفها إلى Chrome أو Brave أو Edge.",
        "Extension folder not found next to the app." => "لم يُعثر على مجلد الإضافة بجانب التطبيق.",
        "How to add" => "طريقة الإضافة",
        "Theme" => "المظهر",
        "Choose how Orexa Downloader looks." => "اختر شكل Orexa Downloader.",
        "Light" => "فاتح",
        "Dark" => "داكن",
        "Choose the default download folder" => "اختر مجلد التنزيل الافتراضي",
        // About
        "About" => "حول",
        "License" => "الترخيص",
        "Free to use" => "مجاني الاستخدام",
        "Orexa Downloader is free to use." => "Orexa Downloader مجاني الاستخدام.",
        "Made with" => "صُنع بـ",
        "by Orexa Software from Egypt" => "بواسطة Orexa Software من مصر",
        // Add dialog
        "Add a download" => "إضافة تنزيل",
        "Paste a link and choose where to save it." => "الصق رابطًا واختر مكان الحفظ.",
        "Close" => "إغلاق",
        "Download link" => "رابط التنزيل",
        "Try again" => "حاول مجددًا",
        "Install ffmpeg" => "تثبيت ffmpeg",
        "Set up now" => "إعداد الآن",
        "Quality" => "الجودة",
        "Size will show once it starts" => "سيظهر الحجم بعد البدء",
        "Sign in to confirm" => "سجّل الدخول للتأكيد",
        "YouTube wants to confirm you are not a bot" => "يريد YouTube التأكد من أنك لست روبوتًا",
        "Open YouTube in Chrome (the Orexa extension shares your sign-in), then press Try again." => {
            "افتح YouTube في Chrome (تشارك إضافة Orexa تسجيل دخولك) ثم اضغط حاول مجددًا."
        }
        "Could not read this video" => "تعذّرت قراءة هذا الفيديو",
        "Video link detected" => "تم اكتشاف رابط فيديو",
        "Reading video details... this can take a little while." => "جارٍ قراءة تفاصيل الفيديو... قد يستغرق ذلك بعض الوقت.",
        "Getting video support ready. This happens once and continues on its own." => {
            "جارٍ تجهيز دعم الفيديو. يحدث هذا مرة واحدة ويستمر تلقائيًا."
        }
        "Getting video support ready..." => "جارٍ تجهيز دعم الفيديو...",
        "Use http or https" => "استخدم http أو https",
        "Link is ready" => "الرابط جاهز",
        "Starts anyway" => "سيبدأ على أي حال",
        "Link expired or blocked" => "الرابط منتهي أو محظور",
        "Could not check link" => "تعذّر فحص الرابط",
        "Save to" => "مكان الحفظ",
        "Browse" => "استعراض",
        "Choose a download folder" => "اختر مجلد التنزيل",
        "A download is running. Add this one to the queue to start after it, or download it now." => {
            "هناك تنزيل قيد التشغيل. أضف هذا إلى قائمة الانتظار ليبدأ بعده، أو نزّله الآن."
        }
        "Start downloading now" => "ابدأ التنزيل الآن",
        "Turn off to add it to the queue instead." => "أوقف الخيار لإضافته إلى قائمة الانتظار بدلًا من ذلك.",
        "Download now" => "نزّل الآن",
        "Add to queue" => "أضف إلى الانتظار",
        "Start download" => "ابدأ التنزيل",
        "Cancel" => "إلغاء",
        "Remove this download?" => "إزالة هذا التنزيل؟",
        "downloaded before" => "نُزّل من قبل",
        "downloading now" => "قيد التنزيل الآن",
        "waiting in the queue" => "في قائمة الانتظار",
        "paused" => "متوقف مؤقتًا",
        "failed earlier" => "فشل سابقًا",
        "Already in your list" => "موجود في قائمتك بالفعل",
        "Added to the queue" => "أُضيف إلى قائمة الانتظار",
        "Download started" => "بدأ التنزيل",
        "Download complete" => "اكتمل التنزيل",
        "Orexa Downloader is still running" => "Orexa Downloader ما زال يعمل",
        "It is in the tray. Click its icon to open it, or choose Quit there." => {
            "هو في شريط النظام. انقر على أيقونته لفتحه، أو اختر خروج من هناك."
        }
        "YouTube video" => "فيديو YouTube",
        // Torrent
        "Progress" => "التقدم",
        "Download speed" => "سرعة التنزيل",
        "Upload speed" => "سرعة الرفع",
        "Sent so far" => "ما أُرسل حتى الآن",
        "Nothing yet" => "لا شيء بعد",
        "Connected to" => "متصل بـ",
        "Peers found" => "المصادر المكتشفة",
        "State" => "الحالة",
        "Finished" => "مكتمل",
        "Where" => "المكان",
        "Torrent ID" => "معرّف التورنت",
        "Files" => "الملفات",
        "All" => "الكل",
        "Tick or untick files while it runs. Files you untick stop downloading." => {
            "حدّد الملفات أو ألغِ تحديدها أثناء التشغيل. الملفات التي تلغي تحديدها يتوقف تنزيلها."
        }
        "This torrent is finished. Live details show while a torrent is downloading or being shared." => {
            "اكتمل هذا التورنت. تظهر التفاصيل الحية أثناء تنزيل التورنت أو مشاركته."
        }
        "Live details show while the torrent is downloading. Start it to see peers, speeds and files." => {
            "تظهر التفاصيل الحية أثناء تنزيل التورنت. ابدأه لرؤية المصادر والسرعات والملفات."
        }
        "Stop sharing" => "إيقاف المشاركة",
        "Apply" => "تطبيق",
        "Only .torrent files can be dropped here" => "يمكن إفلات ملفات .torrent فقط هنا",
        "Use Add URL for other links." => "استخدم إضافة رابط للروابط الأخرى.",
        "Drop a .torrent file to add it" => "أفلت ملف .torrent لإضافته",
        "You can also click a magnet link anywhere." => "يمكنك أيضًا النقر على رابط magnet في أي مكان.",
        // Extension help
        "How to add the extension" => "كيف أضيف الإضافة",
        "Chrome" => "Chrome",
        "Brave" => "Brave",
        "Edge" => "Edge",
        "Open the extensions page" => "افتح صفحة الإضافات",
        "Turn on Developer mode" => "فعّل وضع المطوّر",
        "Use the switch in the top-right corner of the page." => "استخدم المفتاح في الزاوية العلوية اليمنى من الصفحة.",
        "Click Load unpacked" => "انقر على تحميل إضافة غير مضغوطة",
        "A window for choosing a folder opens." => "تفتح نافذة لاختيار مجلد.",
        "Choose the extension folder" => "اختر مجلد الإضافة",
        "Select the folder named \"extension\" (the Open folder button above shows you where it is), then press Select Folder." => {
            "اختر المجلد المسمّى extension (يوضح لك زر فتح المجلد أعلاه مكانه) ثم اضغط اختيار مجلد."
        }
        "Done" => "تم",
        "Orexa Downloader now appears in your list. From now on, downloads, videos and magnet links open Orexa's window. Tip: pin it with the puzzle-piece icon." => {
            "ظهر Orexa Downloader الآن في قائمتك. من الآن ستفتح التنزيلات والفيديوهات وروابط magnet نافذة Orexa. نصيحة: ثبّته بأيقونة قطعة الأحجية."
        }
        "Add the Orexa extension to your browser" => "أضف إضافة Orexa إلى متصفحك",
        "It takes about a minute and you only do it once." => "تستغرق نحو دقيقة وتفعلها مرة واحدة فقط.",
        "Extension folder" => "مجلد الإضافة",
        "Not found next to the app" => "غير موجود بجانب التطبيق",
        "Copy address" => "نسخ العنوان",
        "Using Brave or Edge? The steps are the same; only the address at the start differs." => {
            "تستخدم Brave أو Edge؟ الخطوات نفسها، والفرق فقط في العنوان في البداية."
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_stays_english() {
        set(Lang::En);
        assert_eq!(translate("Settings"), None);
    }

    #[test]
    fn templates_fill_in_values() {
        set(Lang::Ar);
        assert_eq!(translate("3 files").as_deref(), Some("3 ملفات"));
        assert_eq!(translate("2 min 5 s").as_deref(), Some("2 د 5 ث"));
        assert_eq!(translate("1 h 05 min").as_deref(), Some("1 س 05 د"));
        assert_eq!(translate("About 12 MB").as_deref(), Some("حوالي 12 MB"));
        assert_eq!(translate("4.0 MB/s left").as_deref(), Some("متبقي 4.0 MB/s"));
        set(Lang::En);
    }

    #[test]
    fn unknown_text_is_left_alone() {
        set(Lang::Ar);
        assert_eq!(translate("movie.mp4"), None);
        set(Lang::En);
    }
}
