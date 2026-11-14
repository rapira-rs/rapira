<?php
// Every boot logs one record and ends before its first pull, so every boot fails.
\Rapira\log('boot attempt');
