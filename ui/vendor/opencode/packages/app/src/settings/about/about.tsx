import { useDialog } from "@opencode/ui/context/dialog"
import { useLanguage } from "@/runtime/i18n/language"
import { usePlatform } from "@/runtime/platform/platform"
import { ExternalLink } from "@/runtime/platform/external-link"
import DialogThirdPartyNotices from "./notices/dialog"
import { BRAND, EscapeMark } from "@/escape/brand"

export function SettingsAbout() {
  const language = useLanguage()
  const platform = usePlatform()
  const dialog = useDialog()

  const showNotices = () => {
    void dialog.show(() => <DialogThirdPartyNotices />)
  }

  return (
    <div class="settings-about-content">
      <div class="ea-about">
        <div class="ea-about-hero">
          <EscapeMark src={BRAND.mark} class="ea-about-mark" decorative={false} alt={BRAND.name} />
          <div class="ea-about-title">{BRAND.name.toUpperCase()}</div>
          <div class="ea-about-kicker">{language.t("settings.about.kicker")}</div>
          <div class="ea-about-meta">
            <span class="ea-about-pill">
              {language.t("settings.about.version", {
                version: platform.version ?? language.t("settings.about.devVersion"),
              })}
            </span>
            <span class="ea-about-pill">{language.t("settings.about.license")}</span>
          </div>
        </div>

        <div class="ea-about-copy">
          <p>{language.t("settings.about.description")}</p>
          <p>{language.t("settings.about.builtOn")}</p>
        </div>

        <dl class="ea-about-rows">
          <div class="ea-about-row">
            <dt>{language.t("settings.about.repository")}</dt>
            <dd>
              <ExternalLink href={BRAND.repository} class="ea-about-link">
                <bdi dir="ltr">{language.t("settings.about.website")}</bdi>
              </ExternalLink>
            </dd>
          </div>
          <div class="ea-about-row">
            <dt>{language.t("settings.about.licenseLabel")}</dt>
            <dd>{language.t("settings.about.license")}</dd>
          </div>
          <div class="ea-about-row">
            <dt>{language.t("settings.about.typeset")}</dt>
            <dd>Inter · IBM Plex Sans Condensed · IBM Plex Mono</dd>
          </div>
          <div class="ea-about-row">
            <dt>{language.t("settings.about.notices.title")}</dt>
            <dd>
              <button type="button" class="ea-about-link" onClick={() => void showNotices()}>
                {language.t("settings.about.notices.license")}
              </button>
            </dd>
          </div>
        </dl>

        <p class="ea-about-tagline">{language.t("settings.about.tagline")}</p>
        <p class="ea-about-fineprint">{language.t("settings.about.trademark")}</p>
        <p class="ea-about-fineprint">{language.t("settings.about.copyright")}</p>
      </div>
    </div>
  )
}
