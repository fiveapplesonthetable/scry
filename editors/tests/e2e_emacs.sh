#!/usr/bin/env bash
# Headless e2e for editors/emacs/scry.el. Spawns emacs --batch,
# loads the plugin, drives it against a real index, asserts on the
# parsed results. Exit 0 on green, non-zero on red.

set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"

INDEX="${INDEX:-/mnt/agent/tmp/scry-self-idx}"
SCRY="${SCRY:-$root/target/release/scry}"

if [ ! -d "$INDEX" ]; then
    echo "[e2e_emacs] building index of scry's own repo at $INDEX"
    rm -rf "$INDEX"
    "$SCRY" index "$root" -o "$INDEX" --workers 4 > /dev/null
fi

echo "[e2e_emacs] using INDEX=$INDEX SCRY=$SCRY"

emacs --batch \
      -L "$root/editors/emacs" \
      --eval "(progn
  (setq scry-binary \"$SCRY\"
        scry-index-dir \"$INDEX\"
        scry-socket-path (format \"/tmp/scry-e2e-emacs-%d.sock\" (emacs-pid)))
  (require 'scry)
  (let (fails)
    (cl-flet ((ck (label form pred)
                (princ (format \"  %s ... \" label))
                (let ((res (condition-case e (funcall form)
                             (error (cons 'err e)))))
                  (cond ((and (consp res) (eq (car res) 'err))
                         (push (format \"%s: errored %S\" label (cdr res)) fails)
                         (princ \"ERR\\n\"))
                        ((funcall pred res)
                         (princ \"ok\\n\"))
                        (t
                         (push (format \"%s: got %S\" label res) fails)
                         (princ \"FAIL\\n\"))))))
      (ck \"stats\"
          (lambda () (scry--request \"stats\"))
          (lambda (r) (and (listp r)
                       (numberp (alist-get 'symbols r))
                       (> (alist-get 'symbols r) 0))))

      (ck \"prefix returns rows\"
          (lambda () (scry--request \"prefix\"
                                    (list (cons \"prefix\" \"restore\")
                                          (cons \"limit\" 5))))
          (lambda (r) (and (listp r)
                       (cl-find-if (lambda (row)
                                     (string-prefix-p \"restore\"
                                                      (alist-get 'name row)))
                                   r))))

      (ck \"def lands on a real path:line\"
          (lambda () (scry--request \"def\"
                                    (list (cons \"name\" \"compute_id\")
                                          (cons \"limit\" 3))))
          (lambda (r) (and (listp r) r
                       (let ((row (car r)))
                         (and (alist-get 'path row)
                              (numberp (alist-get 'line row))
                              (> (alist-get 'line row) 0))))))

      (ck \"callers returns at least one\"
          (lambda () (scry--request \"callers\"
                                    (list (cons \"name\" \"compute_id\")
                                          (cons \"limit\" 5))))
          (lambda (r) (and (listp r) r
                       (alist-get 'ref_kind (car r)))))

      (ck \"outline of lib.rs has > 10 symbols\"
          (lambda ()
            (scry--request \"outline\"
                           (list (cons \"path\" (concat \"$root/crates/scry-store/src/lib.rs\"))
                                 (cons \"limit\" 200))))
          (lambda (r) (let ((syms (alist-get 'symbols r)))
                        (and (listp syms) (> (length syms) 10)))))

      (ck \"fuzzy substr matches\"
          (lambda () (scry--request \"fuzzy\"
                                    (list (cons \"substr\" \"sigpipe\")
                                          (cons \"limit\" 3))))
          (lambda (r) (and (listp r) r
                       (cl-some (lambda (row)
                                  (string-match-p \"sigpipe\"
                                                  (alist-get 'name row)))
                                r))))

      (ck \"xref-backend-definitions integration\"
          (lambda ()
            (with-temp-buffer
              (insert \"compute_id\")
              (goto-char 1)
              (scry-mode 1)
              (xref-backend-definitions 'scry \"compute_id\")))
          (lambda (r) (and (listp r) r)))

      (ck \"completion-at-point returns CAPF tuple\"
          (lambda ()
            (with-temp-buffer
              (text-mode)
              (insert \"restore\")
              (goto-char (point-max))
              (scry-mode 1)
              (scry-completion-at-point)))
          (lambda (capf)
            ;; CAPF result: (BEG END TABLE . PROPS)
            (and (listp capf)
                 (>= (length capf) 3)
                 (numberp (nth 0 capf))
                 (numberp (nth 1 capf))
                 (functionp (nth 2 capf))
                 (member \"restore_default_sigpipe\"
                         (funcall (nth 2 capf) \"restore\" nil t))))))

    (if fails
        (progn
          (princ \"\\n=== FAILURES ===\\n\")
          (dolist (f fails) (princ (concat \"  \" f \"\\n\")))
          (kill-emacs 1))
      (princ \"\\nALL OK\\n\")
      (kill-emacs 0))))" 2>&1
