# Sleep stages

Ringmaster estimates deep sleep, REM, light sleep and time awake from the RingConn ring's
2.5-minute epochs (`crates/ringmaster/src/stages.rs`). The estimate follows the signs sleep research
describes for each stage. It is not trained on any app's labels and is not a medical measurement;
the README says so.

## What each epoch is judged on

| Stage | Signs | Sources |
|---|---|---|
| Deep | Heart rate lower and steadier than the surrounding 45 minutes, HRV higher, breathing steady, no movement | Penzel et al. 2016; Cabiddu et al. 2012 |
| REM | Heart rate higher and less steady, HRV lower, breathing irregular, no movement | Penzel et al. 2016; Cabiddu et al. 2012 |
| Light | The default when neither shows | |
| Awake | The ring's restless flag or a movement total over 2000 (`sleep::restless`), the same rule as the time awake shown with each sleep | |

Each sign is measured in spreads of that night (interquartile range over 1.35), so "low" means low
for that night. Steadiness for deep sleep and the night as a whole are also compared with the
wearer's previous 14 nights of four hours or more, so a night whose heart rate never settles shows
less deep sleep instead of being rescaled into an ordinary night.

## Timing and order

- Deep sleep is favoured early and REM late: deep sleep falls mostly in the first third of the
  night and REM grows towards morning (Carskadon and Dement).
- No REM in the first 70 minutes; the first REM comes about 80 to 100 minutes after sleep onset
  (Carskadon and Dement).
- Sleep begins in light sleep, deep sleep and REM do not follow each other directly, and waking
  leads to light sleep. A Viterbi pass picks the most likely sequence under these rules, with a
  0.92 chance of staying in a stage from one epoch to the next.

## Amounts

The amount of deep sleep a night gets depends on how strong its signs must be before they count,
and the epochs alone cannot fix that: over 34 nights the median share of deep sleep ran from 0 to
17 % across reasonable cut-offs. The settings were therefore set once so that a typical night lands
near the published shares for healthy adults, about 20 % deep sleep and 19 % REM at ages 35 to 49,
with no significant change with age or sex under current scoring rules (Boulos et al. 2019). They
are the same for every night and every wearer; no night is held to a quota.

On 34 recent nights and 154 nights from the official app's era the median shares came to 19 and
16 % deep sleep and 17 and 19 % REM, with the first REM a median 98 and 104 minutes after sleep
onset. Single nights ranged from 1 to 33 % deep sleep. Raising the heart rate of the same nights by
6 bpm and lowering HRV by 30 % brought the median deep sleep from 16 to 9 %.

Agreement with the official app's stages on the older nights is about half of all epochs. The
official app places deep sleep mostly by time of night, which these estimates do not copy.

## Restless nights

A night is called restless when its deep sleep is under half of the wearer's usual and its median
heart rate is at least 3 bpm above the usual or its median HRV at least 5 ms below it. The note
names the differences.

## Limits

- Lying still while awake reads as sleep, as for every wrist or finger tracker: in a 2025 study of
  six wearables against polysomnography, all detected over 90 % of sleep epochs but only 29 to 52 %
  of waking ones (SLEEP Advances 2025, below).
- Skin temperature is not used: it moves by about 0.5 °C within a night and does not follow heart
  rate on the tested ring, and adding it to an Oura model raised four-stage accuracy only from 57 to
  60 % (Altini and Kinnunen 2021).
- Only RingConn rings: other rings' epochs do not carry the same fields.

## Sources

- Altini M, Kinnunen H. The promise of sleep: a multi-sensor approach for accurate sleep stage
  detection using the Oura ring. Sensors 2021;21(13):4302.
  https://pmc.ncbi.nlm.nih.gov/articles/PMC8271886
- Boulos MI et al. Normal polysomnography parameters in healthy adults: a systematic review and
  meta-analysis. Lancet Respir Med 2019;7(6):533-543. doi:10.1016/S2213-2600(19)30057-8
- Cabiddu R et al. Modulation of the sympatho-vagal balance during sleep: frequency domain study of
  heart rate variability and respiration. Front Physiol 2012;3:45.
  https://pmc.ncbi.nlm.nih.gov/articles/PMC3299415
- Carskadon MA, Dement WC. Normal human sleep: an overview. In: Principles and Practice of Sleep
  Medicine.
- Penzel T et al. Modulations of heart rate, ECG, and cardio-respiratory coupling observed in
  polysomnography. Front Physiol 2016;7:460. https://pmc.ncbi.nlm.nih.gov/articles/PMC5078504
- A performance validation of six commercial wrist-worn wearable sleep-tracking
  devices for sleep stage scoring compared to polysomnography. SLEEP Advances 2025;6(2):zpaf021.
  https://doi.org/10.1093/sleepadvances/zpaf021
