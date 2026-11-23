//! Built-in translations (Portuguese, English, Spanish, French, German).
//! By default we follow the Windows display language, or the user can pick one from the right-click menu.

use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    En,
    Pt,
    Es,
    Fr,
    De,
}

impl Lang {
    pub const ALL: [Lang; 5] = [Lang::En, Lang::Pt, Lang::Es, Lang::Fr, Lang::De];

    pub fn code(self) -> &'static str {
        match self {
            Lang::Pt => "pt",
            Lang::En => "en",
            Lang::Es => "es",
            Lang::Fr => "fr",
            Lang::De => "de",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Lang::Pt => "Português",
            Lang::En => "English",
            Lang::Es => "Español",
            Lang::Fr => "Français",
            Lang::De => "Deutsch",
        }
    }

    pub fn from_code(code: &str) -> Option<Lang> {
        match code.trim().to_ascii_lowercase().as_str() {
            "pt" | "pt-br" | "pt-pt" => Some(Lang::Pt),
            "en" | "en-us" | "en-gb" => Some(Lang::En),
            "es" | "es-es" | "es-mx" => Some(Lang::Es),
            "fr" | "fr-fr" => Some(Lang::Fr),
            "de" | "de-de" => Some(Lang::De),
            _ => None,
        }
    }

    /// Reads Windows' primary UI language ID (`LANGID & 0x3FF`).
    pub fn system_default() -> Lang {
        let lang_id = unsafe { GetUserDefaultUILanguage() };
        match lang_id & 0x3ff {
            0x16 => Lang::Pt, // LANG_PORTUGUESE
            0x0a => Lang::Es, // LANG_SPANISH
            0x0c => Lang::Fr, // LANG_FRENCH
            0x07 => Lang::De, // LANG_GERMAN
            _ => Lang::En,
        }
    }
}

pub struct Texts {
    pub already_running_title: &'static str,
    pub already_running_body: &'static str,
    pub welcome_title: &'static str,
    pub welcome_body: &'static str,
    pub muted: &'static str,
    pub mute_now: &'static str,
    pub unmute_now: &'static str,
    pub output_devices_header: &'static str,
    pub no_output_devices: &'static str,
    pub open_mixer: &'static str,
    pub show_device_name_on_bar: &'static str,
    pub start_with_windows: &'static str,
    pub language_menu: &'static str,
    pub language_auto: &'static str,
    pub check_updates: &'static str,
    pub copy_diagnostics: &'static str,
    pub exit: &'static str,
    pub mixer_master_label: &'static str,
    pub mixer_apps_header: &'static str,
    pub mixer_no_apps: &'static str,
    pub mixer_system_sounds: &'static str,
    pub comm_active_badge: &'static str,
    pub comm_tooltip_prefix: &'static str,
    pub diag_copied_title: &'static str,
    pub diag_copied_body: &'static str,
    pub diag_copy_failed_title: &'static str,
    pub diag_copy_failed_body: &'static str,
    pub diag_empty_title: &'static str,
    pub diag_empty_body: &'static str,
    pub update_checking_title: &'static str,
    pub update_checking_body: &'static str,
    pub update_up_to_date_title: &'static str,
    pub update_up_to_date_body: &'static str,
    pub update_available_title: &'static str,
    pub update_prompt_install: &'static str,
    pub update_installed_title: &'static str,
    pub update_installed_body: &'static str,
    pub update_failed_title: &'static str,
}

pub fn t() -> &'static Texts {
    for_lang(crate::settings::active_lang())
}

pub fn for_lang(lang: Lang) -> &'static Texts {
    match lang {
        Lang::Pt => &PT,
        Lang::En => &EN,
        Lang::Es => &ES,
        Lang::Fr => &FR,
        Lang::De => &DE,
    }
}

const PT: Texts = Texts {
    already_running_title: "O Taskbar Audio já está aberto",
    already_running_body: "O controle de volume já está ali na barra de tarefas, perto do relógio.",
    welcome_title: "Taskbar Audio está rodando",
    welcome_body: "Seu volume fica na barra de tarefas, ao lado do relógio. Role o scroll para ajustar, clique para abrir o mixer ou clique com o botão direito para trocar a saída.",
    muted: "Mudo",
    mute_now: "Silenciar áudio",
    unmute_now: "Ativar som",
    output_devices_header: "Saída de áudio",
    no_output_devices: "Nenhum dispositivo de saída encontrado",
    open_mixer: "Abrir mixer de aplicativos",
    show_device_name_on_bar: "Mostrar nome do dispositivo na barra",
    start_with_windows: "Iniciar com o Windows",
    language_menu: "Idioma",
    language_auto: "Automático (Windows)",
    check_updates: "Verificar atualizações",
    copy_diagnostics: "Copiar diagnóstico",
    exit: "Sair",
    mixer_master_label: "Volume principal",
    mixer_apps_header: "Aplicativos em uso",
    mixer_no_apps: "Nenhum aplicativo reproduzindo áudio agora",
    mixer_system_sounds: "Sons do sistema",
    comm_active_badge: "Em chamada",
    comm_tooltip_prefix: "Modo comunicação ativo",
    diag_copied_title: "Diagnóstico copiado",
    diag_copied_body: "Agora é só colar na issue do GitHub.",
    diag_copy_failed_title: "Não deu para copiar",
    diag_copy_failed_body: "Outro programa está usando a área de transferência. Tente de novo.",
    diag_empty_title: "Sem registros ainda",
    diag_empty_body: "Nada foi gravado no log até o momento.",
    update_checking_title: "Procurando atualizações...",
    update_checking_body: "Consultando o último lançamento no GitHub.",
    update_up_to_date_title: "Tudo em dia!",
    update_up_to_date_body: "Você já está usando a versão mais recente.",
    update_available_title: "Nova versão disponível",
    update_prompt_install: "Encontramos a versão {ver}.\n\nQuer baixar e atualizar agora? (Se escolher 'Não', abriremos a página no GitHub.)",
    update_installed_title: "Atualização concluída",
    update_installed_body: "Reiniciando para aplicar a nova versão...",
    update_failed_title: "Não foi possível verificar atualizações",
};

const EN: Texts = Texts {
    already_running_title: "Taskbar Audio is already running",
    already_running_body: "Look for the volume widget on your taskbar, just left of the system tray.",
    welcome_title: "Taskbar Audio is running",
    welcome_body: "Volume shows right on the taskbar next to the tray. Scroll to change volume, left-click for the app mixer, or right-click to switch outputs.",
    muted: "Muted",
    mute_now: "Mute master volume",
    unmute_now: "Unmute master volume",
    output_devices_header: "Output device",
    no_output_devices: "No audio output devices found",
    open_mixer: "Open app volume mixer",
    show_device_name_on_bar: "Show device name on taskbar",
    start_with_windows: "Start with Windows",
    language_menu: "Language",
    language_auto: "Automatic (Windows)",
    check_updates: "Check for updates",
    copy_diagnostics: "Copy diagnostics",
    exit: "Exit",
    mixer_master_label: "Master volume",
    mixer_apps_header: "Active apps",
    mixer_no_apps: "No apps playing audio right now",
    mixer_system_sounds: "System sounds",
    comm_active_badge: "In call",
    comm_tooltip_prefix: "Communication mode active",
    diag_copied_title: "Diagnostics copied",
    diag_copied_body: "You can paste them directly into a GitHub issue.",
    diag_copy_failed_title: "Couldn't copy diagnostics",
    diag_copy_failed_body: "Another app is holding the clipboard. Try again in a second.",
    diag_empty_title: "No diagnostics yet",
    diag_empty_body: "Nothing has been logged so far.",
    update_checking_title: "Checking for updates...",
    update_checking_body: "Looking up the latest release on GitHub.",
    update_up_to_date_title: "You're up to date",
    update_up_to_date_body: "You're already running the latest version.",
    update_available_title: "Update available",
    update_prompt_install: "Version {ver} is out.\n\nDownload and install it now? (Click 'No' to open the GitHub release page instead.)",
    update_installed_title: "Update installed",
    update_installed_body: "Restarting into the new version...",
    update_failed_title: "Update check failed",
};

const ES: Texts = Texts {
    already_running_title: "Taskbar Audio ya está en execução",
    already_running_body: "El control de volumen ya aparece en la barra de tareas, junto al reloj.",
    welcome_title: "Taskbar Audio está activo",
    welcome_body: "El volumen se muestra en la barra de tareas. Usa la rueda del ratón para ajustarlo, clic izquierdo para el mezclador o clic derecho para cambiar de salida.",
    muted: "Silenciado",
    mute_now: "Silenciar audio",
    unmute_now: "Activar sonido",
    output_devices_header: "Dispositivo de salida",
    no_output_devices: "No se encontraron salidas de audio",
    open_mixer: "Abrir mezclador de aplicaciones",
    show_device_name_on_bar: "Mostrar nombre del dispositivo en la barra",
    start_with_windows: "Iniciar con Windows",
    language_menu: "Idioma",
    language_auto: "Automático (Windows)",
    check_updates: "Buscar actualizaciones",
    copy_diagnostics: "Copiar diagnóstico",
    exit: "Salir",
    mixer_master_label: "Volumen principal",
    mixer_apps_header: "Aplicaciones activas",
    mixer_no_apps: "Ninguna aplicación reproduce audio ahora",
    mixer_system_sounds: "Sonidos del sistema",
    comm_active_badge: "En llamada",
    comm_tooltip_prefix: "Modo comunicación activo",
    diag_copied_title: "Diagnóstico copiado",
    diag_copied_body: "Pégalo en tu reporte en GitHub.",
    diag_copy_failed_title: "No se pudo copiar",
    diag_copy_failed_body: "Otra aplicación está usando el portapapeles. Inténtalo de nuevo.",
    diag_empty_title: "Sin diagnósticos todavía",
    diag_empty_body: "No se ha registrado nada por ahora.",
    update_checking_title: "Buscando actualizaciones...",
    update_checking_body: "Consultando la última versión en GitHub.",
    update_up_to_date_title: "¡Todo al día!",
    update_up_to_date_body: "Ya tienes instalada la versión más reciente.",
    update_available_title: "Nueva versión disponible",
    update_prompt_install: "La versión {ver} está disponible.\n\n¿Descargar y actualizar ahora? (Pulsa 'No' para abrir la página de GitHub.)",
    update_installed_title: "Actualización instalada",
    update_installed_body: "Reiniciando a la nueva versión...",
    update_failed_title: "No se pudo buscar actualizaciones",
};

const FR: Texts = Texts {
    already_running_title: "Taskbar Audio est déjà lancé",
    already_running_body: "Le contrôle du volume se trouve sur la barre des tâches, à gauche de l'horloge.",
    welcome_title: "Taskbar Audio est actif",
    welcome_body: "Le volume s'affiche sur la barre des tâches. Utilisez la molette pour régler le son, clic gauche pour le mixeur ou clic droit pour changer de sortie.",
    muted: "Muet",
    mute_now: "Couper le son",
    unmute_now: "Rétablir le son",
    output_devices_header: "Périphérique de sortie",
    no_output_devices: "Aucune sortie audio détectée",
    open_mixer: "Ouvrir le mixeur d'applications",
    show_device_name_on_bar: "Afficher le nom de la sortie sur la barre",
    start_with_windows: "Démarrer avec Windows",
    language_menu: "Langue",
    language_auto: "Automatique (Windows)",
    check_updates: "Vérifier les mises à jour",
    copy_diagnostics: "Copier le diagnostic",
    exit: "Quitter",
    mixer_master_label: "Volume principal",
    mixer_apps_header: "Applications actives",
    mixer_no_apps: "Aucune application ne joue de son",
    mixer_system_sounds: "Sons système",
    comm_active_badge: "En appel",
    comm_tooltip_prefix: "Mode communication actif",
    diag_copied_title: "Diagnostic copié",
    diag_copied_body: "Collez-le dans votre ticket sur GitHub.",
    diag_copy_failed_title: "Impossible de copier",
    diag_copy_failed_body: "Une autre application utilise le presse-papiers. Réessayez.",
    diag_empty_title: "Aucun journal pour l'instant",
    diag_empty_body: "Rien n'a encore été enregistré.",
    update_checking_title: "Recherche de mises à jour...",
    update_checking_body: "Vérification de la dernière version sur GitHub.",
    update_up_to_date_title: "Vous êtes à jour",
    update_up_to_date_body: "Vous utilisez déjà la dernière version.",
    update_available_title: "Mise à jour disponible",
    update_prompt_install: "La version {ver} est disponible.\n\nTélécharger et installer maintenant ? (Cliquez sur 'Non' pour ouvrir GitHub.)",
    update_installed_title: "Mise à jour installée",
    update_installed_body: "Redémarrage avec la nouvelle version...",
    update_failed_title: "Échec de la vérification des mises à jour",
};

const DE: Texts = Texts {
    already_running_title: "Taskbar Audio läuft bereits",
    already_running_body: "Die Lautstärkeanzeige befindet sich in der Taskleiste links neben der Uhr.",
    welcome_title: "Taskbar Audio ist aktiv",
    welcome_body: "Die Lautstärke steht direkt in der Taskleiste. Mausrad zum Ändern, Linksklick für den App-Mixer, Rechtsklick zum Wechseln des Ausgabegeräts.",
    muted: "Stumm",
    mute_now: "Ton stummschalten",
    unmute_now: "Stummschaltung aufheben",
    output_devices_header: "Ausgabegerät",
    no_output_devices: "Kein Audio-Ausgabegerät gefunden",
    open_mixer: "App-Lautstärkemixer öffnen",
    show_device_name_on_bar: "Gerätenamen in der Taskleiste zeigen",
    start_with_windows: "Mit Windows starten",
    language_menu: "Sprache",
    language_auto: "Automatisch (Windows)",
    check_updates: "Nach Updates suchen",
    copy_diagnostics: "Diagnosedaten kopieren",
    exit: "Beenden",
    mixer_master_label: "Hauptlautstärke",
    mixer_apps_header: "Aktive Apps",
    mixer_no_apps: "Derzeit gibt keine App Ton aus",
    mixer_system_sounds: "Systemklänge",
    comm_active_badge: "Im Anruf",
    comm_tooltip_prefix: "Kommunikationsmodus aktiv",
    diag_copied_title: "Diagnosedaten kopiert",
    diag_copied_body: "Füge sie einfach in dein GitHub-Issue ein.",
    diag_copy_failed_title: "Kopieren fehlgeschlagen",
    diag_copy_failed_body: "Die Zwischenablage wird gerade verwendet. Bitte erneut versuchen.",
    diag_empty_title: "Noch keine Diagnosedaten",
    diag_empty_body: "Bisher wurde nichts protokolliert.",
    update_checking_title: "Suche nach Updates...",
    update_checking_body: "Prüfe das neueste Release auf GitHub.",
    update_up_to_date_title: "Alles aktuell",
    update_up_to_date_body: "Du nutzt bereits die neueste Version.",
    update_available_title: "Update verfügbar",
    update_prompt_install: "Version {ver} ist verfügbar.\n\nJetzt herunterladen und aktualisieren? ('Nein' öffnet die GitHub-Seite.)",
    update_installed_title: "Update installiert",
    update_installed_body: "Neustart mit der neuen Version...",
    update_failed_title: "Update-Prüfung fehlgeschlagen",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_code_roundtrip() {
        for lang in Lang::ALL {
            assert_eq!(Lang::from_code(lang.code()), Some(lang));
        }
        assert_eq!(Lang::from_code("pt-BR"), Some(Lang::Pt));
        assert_eq!(Lang::from_code("en-US"), Some(Lang::En));
        assert_eq!(Lang::from_code("unknown"), None);
    }
}
