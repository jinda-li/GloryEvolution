import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Check, Copy, Loader2, RotateCcw, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

import { createTranslator, resolveLocale } from "../i18n";
import type {
  AppSettings,
  AudioLevelEvent,
  ProcessingProgressEvent,
  RecorderHintEvent,
  RecordingEvent,
  RecordingStatus,
  TextPreviewEvent,
} from "../types";
import { formatClock } from "../utils/format";
import { sounds } from "../utils/sound";

const BAR_COUNT = 13;
const ERROR_AUTO_HIDE_MS = 6000;
const HINT_VISIBLE_MS = 3500;

export function RecorderApp() {
  const [status, setStatus] = useState<RecordingStatus>("recording");
  const [message, setMessage] = useState("");
  const [phase, setPhase] = useState("");
  const [progress, setProgress] = useState(0);
  const [previewText, setPreviewText] = useState("");
  const [copied, setCopied] = useState(false);
  const [elapsed, setElapsed] = useState(0);
  const [levels, setLevels] = useState<number[]>(() => Array(BAR_COUNT).fill(0));
  const [hint, setHint] = useState("");
  const hintTimer = useRef<number | undefined>(undefined);
  const [uiLanguage, setUiLanguage] = useState("auto");
  const soundEnabled = useRef(true);
  const startedAt = useRef(Date.now());
  const statusRef = useRef<RecordingStatus>("idle");

  useEffect(() => {
    document.documentElement.classList.add("recorder-window");
    void invoke<AppSettings>("load_settings").then((settings) => {
      soundEnabled.current = settings.soundEnabled;
      setUiLanguage(settings.uiLanguage ?? "auto");
    });

    const play = (sound: () => void) => {
      if (soundEnabled.current) {
        sound();
      }
    };

    const disposers = [
      listen<RecordingEvent>("recording-status", (event) => {
        const next = event.payload.status;
        const previous = statusRef.current;
        if (next === "recording" && previous !== "recording") {
          play(sounds.start);
        } else if (next === "processing" && previous === "recording") {
          play(sounds.stop);
        } else if (next === "done") {
          play(sounds.done);
        } else if ((next === "error" || next === "failed") && previous !== next) {
          play(sounds.error);
        }
        statusRef.current = next;
        setStatus(next);
        setMessage(event.payload.message);
        if (next === "recording") {
          startedAt.current = Date.now();
          setElapsed(0);
          setPreviewText("");
          setCopied(false);
          setProgress(0);
          setLevels(Array(BAR_COUNT).fill(0));
          setHint("");
        }
      }),
      listen<RecorderHintEvent>("recorder-hint", (event) => {
        setHint(event.payload.message);
        window.clearTimeout(hintTimer.current);
        hintTimer.current = window.setTimeout(() => setHint(""), HINT_VISIBLE_MS);
      }),
      listen<AudioLevelEvent>("audio-level", (event) => {
        const level = Math.max(0, Math.min(1, event.payload.level));
        setLevels((current) => [...current.slice(1), level]);
      }),
      listen<ProcessingProgressEvent>("processing-progress", (event) => {
        setProgress(Math.max(0, Math.min(1, event.payload.progress)));
        setPhase(event.payload.message);
      }),
      listen<TextPreviewEvent>("text-preview", (event) => setPreviewText(event.payload.text)),
      listen<AppSettings>("settings-changed", (event) => {
        soundEnabled.current = event.payload.soundEnabled;
        setUiLanguage(event.payload.uiLanguage ?? "auto");
      }),
    ];

    return () => {
      disposers.forEach((dispose) => void dispose.then((fn) => fn()));
      document.documentElement.classList.remove("recorder-window");
    };
  }, []);

  useEffect(() => {
    if (status !== "recording") {
      return;
    }
    const timer = window.setInterval(() => setElapsed((Date.now() - startedAt.current) / 1000), 250);
    return () => window.clearInterval(timer);
  }, [status]);

  useEffect(() => {
    if (status !== "error") {
      return;
    }
    const timer = window.setTimeout(() => void invoke("hide_recorder"), ERROR_AUTO_HIDE_MS);
    return () => window.clearTimeout(timer);
  }, [status, message]);

  const t = useMemo(() => createTranslator(resolveLocale(uiLanguage)), [uiLanguage]);
  const isRecording = status === "recording";
  const isProcessing = status === "processing";
  const isError = status === "error";
  // Processing failed but the audio is kept: stay open until retried or discarded.
  const isFailed = status === "failed";
  const hasPreview = status === "preview" && Boolean(previewText);

  async function dismiss() {
    if (isRecording) {
      await invoke("cancel_recording");
      return;
    }
    if (isFailed) {
      await invoke("discard_failed");
      return;
    }
    await invoke("hide_recorder");
  }

  async function confirm() {
    if (isRecording) {
      await invoke("stop_and_process");
      return;
    }
    if (isFailed) {
      await invoke("retry_failed");
      return;
    }
    if (hasPreview) {
      await invoke("copy_text", { text: previewText });
      setCopied(true);
      window.setTimeout(() => void invoke("hide_recorder"), 700);
      return;
    }
    await invoke("hide_recorder");
  }

  return (
    <div className="recorder-shell" data-tauri-drag-region>
      <div className={`recorder-pill ${status}`} title={message} data-tauri-drag-region>
        <button
          className="pill-button ghost"
          type="button"
          onClick={() => void dismiss()}
          aria-label={isFailed ? t("recorder.discard") : t("recorder.cancel")}
          title={isFailed ? t("recorder.discard") : undefined}
          disabled={isProcessing}
        >
          <X size={16} />
        </button>

        <div className="pill-body" data-tauri-drag-region>
          {isRecording && hint ? (
            <span className="pill-text hint" data-tauri-drag-region>
              {hint}
            </span>
          ) : null}
          {isRecording && !hint ? (
            <>
              <div className="wave" aria-label={t("recorder.recording")} data-tauri-drag-region>
                {levels.map((level, index) => (
                  <span key={index} style={{ transform: `scaleY(${0.18 + level * 0.82})` }} />
                ))}
              </div>
              <span className="pill-timer">{formatClock(elapsed)}</span>
            </>
          ) : null}
          {isProcessing ? (
            <div className="pill-processing" data-tauri-drag-region>
              <span className="pill-text">
                <Loader2 className="spin" size={14} />
                {phase || t("recorder.processing")}
              </span>
              <div className="pill-progress">
                <span style={{ width: `${Math.round(progress * 100)}%` }} />
              </div>
            </div>
          ) : null}
          {hasPreview ? (
            <div className="pill-preview" data-tauri-drag-region>
              <span className="pill-caption">{copied ? t("recorder.copied") : message}</span>
              <span className="pill-text preview">{previewText}</span>
            </div>
          ) : null}
          {isError ? <span className="pill-text error-text">{message}</span> : null}
          {isFailed ? (
            <div className="pill-preview" data-tauri-drag-region>
              <span className="pill-caption">{t("recorder.failedCaption")}</span>
              <span className="pill-text error-text">{message}</span>
            </div>
          ) : null}
          {status === "done" || status === "idle" ? <span className="pill-text">{message}</span> : null}
        </div>

        <button
          className="pill-button primary"
          type="button"
          onClick={() => void confirm()}
          aria-label={hasPreview ? t("common.copy") : isFailed ? t("recorder.retry") : t("recorder.done")}
          title={isFailed ? t("recorder.retryTitle") : undefined}
          disabled={isProcessing}
        >
          {isFailed ? <RotateCcw size={15} /> : hasPreview && !copied ? <Copy size={15} /> : <Check size={16} />}
        </button>
      </div>
    </div>
  );
}
