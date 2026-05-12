Time Tracker
============

A small time-logging app that lives in your system tray. Start a timer when you
begin a task, stop it when you're done, and it writes the entry to a spreadsheet
for you -- one row per stretch of work. No window to manage; it sits quietly in
the corner of the taskbar.


How to install
--------------

1. In File Explorer, double-click  Install.msix  (the file sitting next to this
   README).
2. Windows App Installer opens. It will show  "Publisher: Ryan Stewart".
3. Click  Install.

That's it. There's no administrator step, no password, and no certificate to
install -- the app is properly signed, so Windows handles it directly.

If a blue box appears that says  "Windows protected your PC":
   - Click  More info
   - Then click  Run anyway
That message is just about download reputation (Windows hasn't seen this file
many times yet) -- it is not a trust problem. The app is signed.


After installing
----------------

- Time Tracker is now in your Start menu. Search for  "Time Tracker"  and open
  it once -- the first launch creates the folder where your time is stored.
- While it's running, a small clock icon sits in the system tray, at the
  bottom-right of the taskbar. On Windows 11 you may need to click the  ^
  chevron next to the clock to see hidden tray icons.


The hotkeys (these work from anywhere -- you don't need the window focused)
--------------------------------------------------------------------------

   Ctrl + Shift + /     Open or close the tracker popover
   Ctrl + Shift + '     Open the workstream filter -- type a few letters, then
                        pick one with the arrow keys + Enter to start its timer
   Ctrl + Shift + ;     Stop the running timer  (writes the entry right away,
                        no confirm prompt)
   Ctrl + Shift + H     Open the popover, ready to add a new workstream

To start timing: open the popover (Ctrl+Shift+/  -- or  Ctrl+Shift+'  to jump
straight to the filter), pick the workstream you're working on, press Enter.
To stop: Ctrl+Shift+; . (There's no separate "start the timer" key -- starting
always goes through picking a workstream in the popover.)

If one of these keys is already used by another program, Time Tracker tells you
on launch. You can change them: open  %LOCALAPPDATA%\TimeTracker\config.toml  in
Notepad and edit the  [hotkeys]  section, then restart Time Tracker. (Heads up:
in that file the two keys are still named  timer_start  and  timer_stop  for
historical reasons -- timer_start's combo is the *stop* hotkey and timer_stop's
combo opens the *filter*; the comments next to them say so.)


Logging a one-off block
-----------------------

Right-click the tray clock icon and choose  "Log a block..."  -- enter the
client, what you worked on, and how long (e.g.  1.5h  or  90m ).


Where your data lives
---------------------

   %USERPROFILE%\TimeTracker\<year>-<month>.csv

One file per month -- it opens in Excel. One tip: don't leave that file open in
Excel while the timer is running. Excel locks the file, so the app holds your
entry in a queue and writes it the moment Excel lets go. (Nothing is lost --
it just waits.)

To see your recorded time and the billing export, use the "Recorded Time" /
"Export" pages in the popover, or open  http://localhost:17893/recorded  in a
browser while Time Tracker is running.


To uninstall later
------------------

Start menu -> right-click  Time Tracker  -> Uninstall  (or Settings -> Apps ->
Time Tracker -> Uninstall). Your logged time and your settings are kept.


If something's wrong
--------------------

Contact Ryan:  <phone>  /  <email>
