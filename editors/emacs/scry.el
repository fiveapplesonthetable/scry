;;; scry.el --- Lightning-fast code intel via scry  -*- lexical-binding: t -*-

;; Copyright (C) 2026  scry contributors
;; License: Apache-2.0
;; URL: https://github.com/fiveapplesonthetable/scry
;; Package-Requires: ((emacs "29.1"))
;; Keywords: tools, languages

;;; Commentary:
;;
;; Editor binding for scry, a static-binary code-search index for
;; AOSP-sized trees.  Provides:
;;
;;   * `completion-at-point' provider — autocomplete on the symbol
;;     at point, ranked, with kind / lang / file annotations.
;;   * xref backend — M-. jumps to definition, M-? lists references
;;     using the standard `xref' commands; no extra keybindings.
;;   * Interactive commands: `scry-def', `scry-callers', `scry-ref',
;;     `scry-outline', `scry-prefix', `scry-fuzzy', `scry-stats'.
;;
;; Talks to a single long-lived `scry serve' subprocess over a
;; line-delimited JSON-RPC socket.  Per-query latency on a warm
;; index is sub-10 ms — fast enough for keystroke-driven completion.
;;
;; Quickstart:
;;
;;   (require 'scry)
;;   (setq scry-index-dir "/mnt/agent/scry-index")  ;; or wherever
;;   (scry-mode 1)            ;; enable globally
;;   ;; or, per file:
;;   (add-hook 'prog-mode-hook #'scry-mode)
;;
;; Then type away.  M-. jumps; M-? finds refs; completion-at-point
;; runs scry prefix.

;;; Code:

(require 'cl-lib)
(require 'json)
(require 'xref)

(defgroup scry nil
  "Code-search via the scry static binary."
  :group 'tools
  :prefix "scry-")

(defcustom scry-binary "scry"
  "Path to the `scry' binary.  Resolved via `executable-find' if relative."
  :type 'string)

(defcustom scry-index-dir nil
  "Path to the scry index directory.
If nil, scry's own default applies (\"/mnt/agent/scry-index\" or
$SCRY_INDEX).  Set this for any project that doesn't live in the
default index."
  :type '(choice (const nil) directory))

(defcustom scry-socket-path
  (format "/tmp/scry-emacs-%s.sock" (user-uid))
  "Unix socket path used to talk to the per-Emacs `scry serve' daemon.
Uses the UID so two users on the same host don't collide."
  :type 'string)

(defcustom scry-max-completions 50
  "Maximum number of candidates `scry prefix' returns for autocomplete."
  :type 'integer)

(defcustom scry-request-timeout 5.0
  "Hard cap on a single JSON-RPC request in seconds."
  :type 'number)

(defcustom scry-completion-min-length 2
  "Minimum prefix length before completion fires."
  :type 'integer)

;;; ----------------------------------------------------------------
;;; Process + socket management
;;; ----------------------------------------------------------------

(defvar scry--server-proc nil
  "The `scry serve' subprocess, or nil if not started.")

(defvar scry--client-proc nil
  "The Emacs-side network process talking to the daemon's socket.")

(defvar scry--pending (make-hash-table :test 'eql)
  "Map id -> result-cell awaiting a response.  See `scry--request'.")

(defvar scry--next-id 1
  "Next JSON-RPC request id to assign.")

(defvar scry--buf ""
  "Receive buffer; line-delimited JSON accumulates here.")

(defun scry--log (fmt &rest args)
  "Log MESSAGE prefixed with [scry]."
  (apply #'message (concat "[scry] " fmt) args))

(defun scry--ensure-server ()
  "Start `scry serve' if it isn't already running, and connect to it."
  (when (and scry--server-proc
             (not (process-live-p scry--server-proc)))
    (setq scry--server-proc nil
          scry--client-proc nil))
  (unless (process-live-p scry--server-proc)
    (when (file-exists-p scry-socket-path)
      (delete-file scry-socket-path))
    (let* ((bin (or (executable-find scry-binary) scry-binary))
           (args (append (list "serve"
                               "--listen" (concat "unix:" scry-socket-path)
                               "--max-conns" "4")
                         (when scry-index-dir
                           (list "--index" scry-index-dir)))))
      (setq scry--server-proc
            (make-process
             :name "scry-serve"
             :buffer (get-buffer-create "*scry-serve*")
             :command (cons bin args)
             :noquery t
             :sentinel #'scry--server-sentinel))
      ;; Wait up to 5 s for the socket to appear.
      (let ((t0 (current-time)))
        (while (and (not (file-exists-p scry-socket-path))
                    (< (float-time (time-since t0)) 5.0))
          (accept-process-output scry--server-proc 0.05)))
      (unless (file-exists-p scry-socket-path)
        (error "scry serve did not bind %s within 5 s" scry-socket-path))))
  (unless (and scry--client-proc (process-live-p scry--client-proc))
    (setq scry--client-proc
          (make-network-process
           :name "scry-client"
           :family 'local
           :service scry-socket-path
           :coding 'utf-8
           :nowait nil
           :filter #'scry--client-filter
           :sentinel #'scry--client-sentinel
           :noquery t))
    (setq scry--buf ""
          scry--pending (make-hash-table :test 'eql))))

(defun scry--server-sentinel (_proc event)
  "Sentinel for the daemon subprocess."
  (scry--log "scry serve: %s" (string-trim event)))

(defun scry--client-sentinel (_proc event)
  "Sentinel for the client socket."
  (when (string-match-p "\\(deleted\\|finished\\|broken\\|closed\\)" event)
    (setq scry--client-proc nil)
    ;; Reject any pending requests so callers don't hang forever.
    (maphash (lambda (_id cell)
               (setf (alist-get 'error cell) "scry socket closed"))
             scry--pending)
    (clrhash scry--pending)))

(defun scry--client-filter (_proc chunk)
  "Append CHUNK to the receive buffer and drain complete JSON lines."
  (setq scry--buf (concat scry--buf chunk))
  (let (line)
    (while (let ((nl (string-match-p "\n" scry--buf)))
             (when nl
               (setq line (substring scry--buf 0 nl)
                     scry--buf (substring scry--buf (1+ nl)))
               t))
      (when (> (length line) 0)
        (scry--deliver line)))))

(defun scry--deliver (line)
  "Parse LINE as a JSON-RPC response and resolve the matching pending request.

Uses the pure-Lisp `json-read-from-string' rather than the native
`json-parse-string' because scry emits u64 symbol IDs (blake3
truncated) that overflow int64.  json.el's parser hands those back
as Emacs bignums; json-parse-string rejects them."
  (let* ((obj (condition-case e
                  (let ((json-object-type 'alist)
                        (json-array-type 'list)
                        (json-key-type 'symbol)
                        (json-null nil)
                        (json-false nil))
                    (json-read-from-string line))
                (error (scry--log "bad JSON: %S" e) nil)))
         (id  (and obj (alist-get 'id obj)))
         (cell (and id (gethash id scry--pending))))
    (when cell
      (remhash id scry--pending)
      (setf (alist-get 'response cell) obj))))

;;; ----------------------------------------------------------------
;;; Request / response
;;; ----------------------------------------------------------------

(defun scry--request (cmd &optional args)
  "Send CMD with ARGS to the daemon synchronously; return parsed response.
Returns the `result' alist on success or signals `error' on failure."
  (scry--ensure-server)
  (let* ((id (cl-incf scry--next-id))
         (req (list (cons "id" id) (cons "cmd" cmd)))
         (cell (list (cons 'response nil))))
    (when args (push (cons "args" args) req))
    (puthash id cell scry--pending)
    (process-send-string scry--client-proc
                         (concat (json-encode req) "\n"))
    (let ((deadline (+ (float-time) scry-request-timeout)))
      (while (and (not (alist-get 'response cell))
                  (< (float-time) deadline))
        (accept-process-output scry--client-proc 0.01))
      (let ((resp (alist-get 'response cell)))
        (unless resp
          (remhash id scry--pending)
          (error "scry request timed out (cmd=%s)" cmd))
        (when (alist-get 'error resp)
          (error "scry: %s" (alist-get 'error resp)))
        (alist-get 'result resp)))))

;;; ----------------------------------------------------------------
;;; Symbol-at-point helpers
;;; ----------------------------------------------------------------

(defun scry--symbol-at-point ()
  "The identifier under point, or nil."
  (let ((sym (thing-at-point 'symbol t)))
    (and sym (string-match-p "\\`[A-Za-z_][A-Za-z0-9_]*\\'" sym) sym)))

(defun scry--lang-for-buffer ()
  "Best-effort language hint for the current buffer, or nil."
  (when buffer-file-name
    (pcase (file-name-extension buffer-file-name)
      ("rs" "Rust") ("go" "Go") ("py" "Python")
      ("c"  "C")   ((or "cc" "cpp" "cxx" "C") "Cpp")
      ((or "h" "hh" "hpp" "hxx") "Header")
      ("java" "Java") ((or "kt" "kts") "Kotlin")
      ("ts" "TypeScript") ("tsx" "TypeScript")
      ("proto" "Proto")
      ("sh" "Bash") ("bash" "Bash")
      ("html" "Html") ("htm" "Html")
      ("css" "Css") ("scss" "Scss")
      ("md" "Markdown")
      ("toml" "Toml") ((or "yaml" "yml") "Yaml")
      (_ nil))))

(defun scry--row-to-xref (row)
  "Turn a scry result ROW into an `xref-item'."
  (let* ((path (alist-get 'path row))
         (line (alist-get 'line row))
         (col  (or (alist-get 'col row) 1))
         (name (alist-get 'name row))
         (kind (alist-get 'kind row))
         (lang (alist-get 'lang row))
         (loc  (xref-make-file-location path line (max 0 (1- col))))
         (summary (format "%-8s %-6s  %s" (or kind "?") (or lang "?") name)))
    (xref-make summary loc)))

;;; ----------------------------------------------------------------
;;; xref backend
;;; ----------------------------------------------------------------

(defun scry--xref-backend () 'scry)

(cl-defmethod xref-backend-identifier-at-point ((_b (eql scry)))
  (scry--symbol-at-point))

(cl-defmethod xref-backend-definitions ((_b (eql scry)) identifier)
  (let* ((args (list (cons "name" identifier)
                     (cons "limit" 25)))
         (lang (scry--lang-for-buffer)))
    (when lang (push (cons "lang" lang) args))
    (mapcar #'scry--row-to-xref
            (scry--request "def" args))))

(cl-defmethod xref-backend-references ((_b (eql scry)) identifier)
  (let* ((args (list (cons "name" identifier)
                     (cons "limit" 200)))
         (lang (scry--lang-for-buffer)))
    (when lang (push (cons "lang" lang) args))
    (mapcar #'scry--row-to-xref
            (scry--request "callers" args))))

(cl-defmethod xref-backend-apropos ((_b (eql scry)) pattern)
  (mapcar #'scry--row-to-xref
          (scry--request "fuzzy"
                         (list (cons "substr" pattern)
                               (cons "limit" 50)))))

(cl-defmethod xref-backend-identifier-completion-table ((_b (eql scry)))
  ;; Programmatic-completion table that defers to scry prefix.
  (lambda (string pred action)
    (let* ((rows (and (>= (length string) scry-completion-min-length)
                      (scry--request "prefix"
                                     (list (cons "prefix" string)
                                           (cons "limit"
                                                 scry-max-completions)))))
           (names (delete-dups
                   (mapcar (lambda (r) (alist-get 'name r)) rows))))
      (complete-with-action action names string pred))))

;;; ----------------------------------------------------------------
;;; completion-at-point provider
;;; ----------------------------------------------------------------

(defun scry-completion-at-point ()
  "Return a CAPF entry backed by `scry prefix'.

When the current symbol is at least `scry-completion-min-length'
characters long, runs `scry prefix' and offers the returned
names as completions.  Each candidate is annotated with its kind
and language."
  (let* ((bounds (bounds-of-thing-at-point 'symbol)))
    (when (and bounds
               (>= (- (cdr bounds) (car bounds)) scry-completion-min-length))
      (let* ((beg (car bounds))
             (end (cdr bounds))
             ;; Cache the lookup so the same prefix in the same call
             ;; doesn't re-issue the request when Emacs tries multiple
             ;; predicates.
             (cache nil)
             (rows-for (lambda (str)
                         (or cache
                             (setq cache
                                   (condition-case _e
                                       (scry--request
                                        "prefix"
                                        (list (cons "prefix" str)
                                              (cons "limit"
                                                    scry-max-completions)))
                                     (error nil))))))
             (table (lambda (string pred action)
                      (let* ((rows (funcall rows-for string))
                             (names (delete-dups
                                     (mapcar (lambda (r) (alist-get 'name r))
                                             rows))))
                        (complete-with-action action names string pred))))
             (annotate (lambda (name)
                         (let ((row (cl-find name (funcall rows-for
                                                           (buffer-substring-no-properties
                                                            beg end))
                                             :key (lambda (r) (alist-get 'name r))
                                             :test #'equal)))
                           (when row
                             (format "  [%s %s]  %s"
                                     (or (alist-get 'kind row) "?")
                                     (or (alist-get 'lang row) "?")
                                     (file-name-nondirectory
                                      (or (alist-get 'path row) ""))))))))
        (list beg end table
              :exclusive 'no
              :annotation-function annotate
              :company-kind
              (lambda (name)
                (let ((row (cl-find name (funcall rows-for
                                                  (buffer-substring-no-properties
                                                   beg end))
                                    :key (lambda (r) (alist-get 'name r))
                                    :test #'equal)))
                  (pcase (and row (alist-get 'kind row))
                    ("class" 'class) ("iface" 'interface) ("struct" 'struct)
                    ("enum" 'enum) ("fn" 'function) ("method" 'method)
                    ("field" 'field) ("var" 'variable) ("const" 'constant)
                    ("module" 'module) ("ns" 'module) ("ctor" 'constructor)
                    (_ 'text))))
              :company-location
              (lambda (name)
                (let ((row (cl-find name (funcall rows-for
                                                  (buffer-substring-no-properties
                                                   beg end))
                                    :key (lambda (r) (alist-get 'name r))
                                    :test #'equal)))
                  (when row
                    (cons (alist-get 'path row)
                          (alist-get 'line row)))))
              ;; corfu + company both recognize :company-doc-buffer for
              ;; popup-side documentation. We render the symbol's full
              ;; FQN, kind, lang, and path in a small read-only buffer
              ;; users can then jump out to via `M-.` (which works
              ;; because xref shares the same scry backend).
              :company-doc-buffer
              (lambda (name)
                (let* ((row (cl-find name (funcall rows-for
                                                   (buffer-substring-no-properties
                                                    beg end))
                                     :key (lambda (r) (alist-get 'name r))
                                     :test #'equal))
                       (buf (get-buffer-create " *scry-doc*")))
                  (when row
                    (with-current-buffer buf
                      (let ((inhibit-read-only t))
                        (erase-buffer)
                        (insert (format "%s\n%s · %s"
                                        (or (alist-get 'fqn row)
                                            (alist-get 'name row))
                                        (or (alist-get 'kind row) "?")
                                        (or (alist-get 'lang row) "?")))
                        (when (alist-get 'scope row)
                          (insert (format " · %s"
                                          (mapconcat #'identity
                                                     (alist-get 'scope row)
                                                     "::"))))
                        (insert (format "\n%s:%s"
                                        (or (alist-get 'path row) "?")
                                        (or (alist-get 'line row) "?")))
                        (special-mode))
                      buf))))
              ;; Single-candidate selection trigger for corfu/company.
              ;; Always allow the popup to surface — the user picks.
              :company-prefix-length t)))))

;;; ----------------------------------------------------------------
;;; Interactive commands
;;; ----------------------------------------------------------------

(defun scry--prompt-symbol (prompt)
  "Read a symbol with default from `thing-at-point'."
  (let ((def (scry--symbol-at-point)))
    (read-string (format "%s%s: " prompt (if def (format " (%s)" def) ""))
                 nil nil def)))

;;;###autoload
(defun scry-def (name)
  "Show definitions of NAME (default: symbol at point) in an xref buffer."
  (interactive (list (scry--prompt-symbol "scry def")))
  (let* ((args (list (cons "name" name) (cons "limit" 25)))
         (lang (scry--lang-for-buffer)))
    (when lang (push (cons "lang" lang) args))
    (let ((xrefs (mapcar #'scry--row-to-xref (scry--request "def" args))))
      (if xrefs (xref-show-xrefs xrefs nil)
        (scry--log "no definitions for %s" name)))))

;;;###autoload
(defun scry-callers (name)
  "Show call sites for NAME (default: symbol at point) in an xref buffer."
  (interactive (list (scry--prompt-symbol "scry callers")))
  (let* ((args (list (cons "name" name) (cons "limit" 200)))
         (lang (scry--lang-for-buffer)))
    (when lang (push (cons "lang" lang) args))
    (let ((xrefs (mapcar #'scry--row-to-xref (scry--request "callers" args))))
      (if xrefs (xref-show-xrefs xrefs nil)
        (scry--log "no callers for %s" name)))))

;;;###autoload
(defun scry-ref (name)
  "Show every reference to NAME, not just calls."
  (interactive (list (scry--prompt-symbol "scry ref")))
  (let* ((args (list (cons "name" name) (cons "limit" 500)))
         (lang (scry--lang-for-buffer)))
    (when lang (push (cons "lang" lang) args))
    (let ((xrefs (mapcar #'scry--row-to-xref (scry--request "ref" args))))
      (if xrefs (xref-show-xrefs xrefs nil)
        (scry--log "no refs for %s" name)))))

;;;###autoload
(defun scry-outline ()
  "Show every symbol defined in the current buffer (LSP documentSymbol)."
  (interactive)
  (unless buffer-file-name
    (user-error "Buffer is not visiting a file"))
  (let* ((path buffer-file-name)
         (result (scry--request "outline"
                                (list (cons "path" path)
                                      (cons "limit" 1000))))
         (rows (alist-get 'symbols result))
         (xrefs (mapcar #'scry--row-to-xref rows)))
    (if xrefs (xref-show-xrefs xrefs nil)
      (scry--log "outline empty for %s" path))))

;;;###autoload
(defun scry-prefix (prefix)
  "Show prefix-completion candidates for PREFIX."
  (interactive (list (read-string "scry prefix: " (scry--symbol-at-point))))
  (let ((xrefs (mapcar #'scry--row-to-xref
                       (scry--request "prefix"
                                      (list (cons "prefix" prefix)
                                            (cons "limit" 100))))))
    (if xrefs (xref-show-xrefs xrefs nil)
      (scry--log "no prefix matches for %s" prefix))))

;;;###autoload
(defun scry-fuzzy (substr)
  "Fuzzy substring search SUBSTR."
  (interactive (list (read-string "scry fuzzy: " (scry--symbol-at-point))))
  (let ((xrefs (mapcar #'scry--row-to-xref
                       (scry--request "fuzzy"
                                      (list (cons "substr" substr)
                                            (cons "limit" 50))))))
    (if xrefs (xref-show-xrefs xrefs nil)
      (scry--log "no fuzzy matches for %s" substr))))

;;;###autoload
(defun scry-stats ()
  "Show daemon-side stats in the message area."
  (interactive)
  (let ((s (scry--request "stats")))
    (scry--log "%s · %s files · %s syms · %s refs · indexed_at=%s"
               (alist-get 'scry_version s)
               (alist-get 'files_total s)
               (alist-get 'symbols s)
               (alist-get 'refs s)
               (alist-get 'indexed_at s))))

;;;###autoload
(defun scry-restart ()
  "Tear down and respawn the scry daemon."
  (interactive)
  (when (process-live-p scry--client-proc)
    (delete-process scry--client-proc))
  (when (process-live-p scry--server-proc)
    (delete-process scry--server-proc))
  (setq scry--client-proc nil
        scry--server-proc nil
        scry--buf "")
  (scry--ensure-server)
  (scry--log "restarted"))

;;; ----------------------------------------------------------------
;;; Minor mode
;;; ----------------------------------------------------------------

;;;###autoload
(define-minor-mode scry-mode
  "Minor mode wiring scry as the xref + CAPF backend for this buffer."
  :lighter " scry"
  (if scry-mode
      (progn
        (add-hook 'xref-backend-functions #'scry--xref-backend nil t)
        (add-hook 'completion-at-point-functions
                  #'scry-completion-at-point nil t))
    (remove-hook 'xref-backend-functions #'scry--xref-backend t)
    (remove-hook 'completion-at-point-functions
                 #'scry-completion-at-point t)))

;;;###autoload
(define-globalized-minor-mode global-scry-mode scry-mode
  (lambda () (when (derived-mode-p 'prog-mode) (scry-mode 1))))

(provide 'scry)

;;; scry.el ends here
