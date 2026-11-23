-- Titles derived before prompt markup was stripped. Clearing them lets the
-- next scan derive a clean title from the transcript.
UPDATE sessions SET title = NULL
WHERE title LIKE '[Image #%'
   OR title LIKE '[Image: %'
   OR title LIKE '[Pasted text #%'
   OR title LIKE '<%';
