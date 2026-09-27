# So you want to mine bitcoin

This page is for people who have a miner, or are thinking about buying one,
and want to know what they are getting into. It covers what mining is, what a
home miner can expect from it, and what running your own pool does for the
network. None of it is specific to this pool until the last part.

The 2008 financial crisis had me asking what money really is. When Bitcoin
showed up on [Slashdot in July 2010](https://news.slashdot.org/story/10/07/11/1747245/bitcoin-releases-version-03), I came to it from cryptography. People
were still arguing over stronger algorithms after MD5 was broken, and I
wanted to see how Bitcoin used hash functions and how each block's hash tied
it to the one before. That is when the two questions started to connect. I
tinkered with it for several years while life kept me busy. It finally
clicked when I listened to Michael Saylor on Robert Breedlove's
[*What is Money?*](https://whatismoneypodcast.com/) podcast
([the Saylor Series](https://www.youtube.com/playlist?list=PL2jAZ0x9H0bQFY6wIbQfnrnIlqMcSHd6X))
and read Saifedean Ammous's
[*The Bitcoin Standard*](https://saifedean.com/the-bitcoin-standard/). This page covers
the mechanics and the money, in that order.

Satoshi Nakamoto described mining plainly in the first emails about Bitcoin
and in the whitepaper, so this page quotes them where they help. Every quote
links to its source.

## What mining is for

On October 31, 2008, Satoshi announced Bitcoin to the Cryptography Mailing
List. Two lines of that email name mining's two jobs:

> New coins are made from Hashcash style proof-of-work.
> The proof-of-work for new coin generation also powers the
> network to prevent double-spending.
>
> Satoshi Nakamoto, [October 31, 2008](https://satoshi.nakamotoinstitute.org/emails/cryptography/1/)

The second job is the one the system depends on. Digital money has one hard
problem: stopping someone from spending the same coins twice. A bank solves
it by keeping the ledger itself. Bitcoin has nobody in that role, so the
network needs another way for everyone to agree which transactions happened,
and in what order, without trusting anyone to say so. Proof of work is that
way:

> The proof-of-work chain is the solution to the synchronisation problem, and
> to knowing what the globally shared view is without having to trust anyone.
>
> Satoshi Nakamoto, [November 9, 2008](https://satoshi.nakamotoinstitute.org/emails/cryptography/7/)

Miners do the work. New coins and fees are how they get paid for it.

## What proof of work is

Every block starts with a template: a candidate block holding a set of
transactions from the mempool plus one special transaction, the coinbase,
that pays the reward to whoever finds the block. Mining is trying to find a
header for that template whose hash falls below a target. There is no
shortcut. A miner changes a few bytes, hashes, checks, and repeats, trillions
of times a second.

Finding a valid hash is hard. Checking one is instant. So any node can
confirm that a block cost real work without trusting whoever sent it.

Each block also includes the hash of the block before it. Changing an old
block means redoing its work and the work of every block since, faster than
everyone else is extending the chain. That is what makes the history
expensive to rewrite, and it is why the network follows the chain with the
most work behind it:

> Proof-of-work is essentially one-CPU-one-vote. The majority decision is
> represented by the longest chain, which has the greatest proof-of-work
> effort invested in it.
>
> Satoshi Nakamoto, [Bitcoin whitepaper, section 4](https://nakamotoinstitute.org/library/bitcoin/)

CPUs gave way to GPUs and then to ASICs, chips that do nothing but this one
hash. The vote is now counted in hashes, but the rule is the same.

The target is not fixed:

> To compensate for increasing hardware speed and varying interest in running
> nodes over time, the proof-of-work difficulty is determined by a moving
> average targeting an average number of blocks per hour. If they're generated
> too fast, the difficulty increases.
>
> Satoshi Nakamoto, [Bitcoin whitepaper, section 4](https://nakamotoinstitute.org/library/bitcoin/)

In practice the network retargets every 2016 blocks so that, across all
miners combined, a block is found about every ten minutes. When more hashrate
joins, it gets harder. When hashrate leaves, it gets easier.

## Old parts, put together

Like the iPhone, Bitcoin was built mostly from parts that already existed.
The whitepaper cites them:

- Ralph Merkle's hash trees (1980), which let one hash stand for every
  transaction in a block.
- Stuart Haber and W. Scott Stornetta's linked timestamps (1991), where each
  record carries the hash of the one before it.
- Adam Back's [Hashcash](http://www.hashcash.org/papers/hashcash.pdf), a
  proof of work first meant to make spam expensive.
- Wei Dai's [b-money](http://www.weidai.com/bmoney.txt) (1998), in which
  "anyone can create money by broadcasting the solution to a previously
  unsolved computational problem."

Satoshi said as much in July 2010, days after the Slashdot story:

> Bitcoin is an implementation of Wei Dai's b-money proposal on Cypherpunks
> in 1998 and Nick Szabo's
> [Bitgold](http://unenumerated.blogspot.com/2005/12/bit-gold.html) proposal
>
> Satoshi Nakamoto, [July 20, 2010](https://satoshi.nakamotoinstitute.org/posts/bitcointalk/249/)

The ingenuity was in how the pieces fit. Proof of work decides which history
is real. The chain of hashes makes that history expensive to rewrite. The
reward pays strangers to do the work. None of it needs anyone in charge.

It also needed its moment. When a skeptic on the mailing list worried about
the bandwidth if hundreds of millions of people used it, Satoshi worked the
numbers at Visa's volume and bet on the technology catching up:

> If the network were to get that big, it would take several years, and by
> then, sending 2 HD movies over the Internet would probably not seem like a
> big deal.
>
> Satoshi Nakamoto, [November 3, 2008](https://satoshi.nakamotoinstitute.org/emails/cryptography/2/)

Cheap computers, broadband, and open-source software made it practical for
anyone to run a node. Today a full node, a pool, and a few miners fit on a
home network.

## The reward

The coinbase transaction is how new coins enter circulation. The Bitcoin 0.1
release announcement laid out the schedule:

> Total circulation will be 21,000,000 coins. It'll be distributed
> to network nodes when they make blocks, with the amount cut in half
> every 4 years.
>
> Satoshi Nakamoto, [January 8, 2009](https://satoshi.nakamotoinstitute.org/emails/cryptography/16/)

The code halves the subsidy every 210,000 blocks, which works out to about
four years. It started at 50 BTC per block. It has been 3.125 BTC since the
April 2024 halving and drops to 1.5625 BTC at block 1,050,000, expected in
2028. The last new coins are expected around 2140.

The whitepaper compares the subsidy to gold mining, and names the cost:

> The steady addition of a constant of amount of new coins is analogous to
> gold miners expending resources to add gold to circulation. In our case, it
> is CPU time and electricity that is expended.
>
> Satoshi Nakamoto, [Bitcoin whitepaper, section 6](https://nakamotoinstitute.org/library/bitcoin/)

The block's other income is fees. Every transaction pays one, and the miner
who includes it collects it. As the subsidy shrinks, fees are meant to take
over:

> There will be transaction fees, so nodes will have an incentive to receive
> and include all the transactions they can. Nodes will eventually be
> compensated by transaction fees alone when the total coins created hits the
> pre-determined ceiling.
>
> Satoshi Nakamoto, [November 15, 2008](https://satoshi.nakamotoinstitute.org/emails/cryptography/13/)

The subsidy was the incentive at the start. Fees are meant to carry it
forward, and whether they will remains to be seen. For now they are a small
part of the reward: in the year to September 2026, fees averaged about 0.02
BTC per block, under 1% of what miners earned. Each halving makes that gap
matter more. What secures the chain is what the reward is worth, so the
price matters as much as the fee total. Any node can check the current
split with
[`bitcoin-cli getblockstats`](https://developer.bitcoin.org/reference/rpc/getblockstats.html).

The miner who finds a block collects the subsidy and all of its fees.
Everyone else gets nothing for that block.

## What you can expect

The same 0.1 announcement set expectations for the first miners:

> I made the proof-of-work difficulty ridiculously easy to start with, so
> for a little while in the beginning a typical PC will be able to
> generate coins in just a few hours. It'll get a lot harder when
> competition makes the automatic adjustment drive up the difficulty.
>
> Satoshi Nakamoto, [January 8, 2009](https://satoshi.nakamotoinstitute.org/emails/cryptography/16/)

It did. To see how much, start with how the odds work, which Satoshi
explained in an earlier email:

> It's a memoryless process where you do millions of hashes a second, with a
> small chance of finding one each time. [...] Anyone's chance of finding a
> solution at any time is proportional to their CPU power.
>
> Satoshi Nakamoto, [November 15, 2008](https://satoshi.nakamotoinstitute.org/emails/cryptography/13/)

"Memoryless" is the key word. Your chance of finding the next block is your
hashrate divided by the network's, every moment you mine. There is no
progress bar, and hours already spent mining do not make the next block more
likely.

This is what "a lot harder" looks like with the network at about 940 EH/s
(the dashboard shows the live figure):

| Hardware | Hashrate | Chance per year | Expected wait |
|---|---|---|---|
| [Bitaxe](https://bitaxe.org/) class | 1.2 TH/s | about 1 in 15,000 | about 15,000 years |
| [NerdQAxe++](https://github.com/shufps/ESP-Miner-NerdQAxePlus) class | 4.8 TH/s | about 1 in 3,700 | about 3,700 years |
| A few home ASICs | 30 TH/s | about 1 in 600 | about 600 years |
| Current large ASIC | 200 TH/s | about 1 in 90 | about 90 years |

To check any row: expected blocks per day is `144 × your hashrate ÷ network
hashrate`. The Block odds card on the [dashboard](../README.md#dashboard--metrics) runs the same numbers for
your actual hashrate.

"Expected wait" is an average, not a schedule. Some solo miners find a block
in their first month. Most never find one. Both outcomes are normal.

The Block odds card also compares your daily chance with a Powerball
ticket's 1 in 292,201,338. A Bitaxe comes out about 54 times better, at
roughly 1 in 5.4 million a day. The comparison is tongue-in-cheek. Better
than Powerball is still a long shot, and solo mining is not a way to get
rich.

The network also grows. When the difficulty rises, your share of it shrinks,
so a device's odds fall over its lifetime unless you add more hashrate.

### The money

Solo mining pays nothing until it pays everything. If steady income is the
goal, a pooled service is the right tool: it combines many miners' work and
pays each a small, regular amount. Your expected earnings are the same either
way (minus the pool's fee); pooling only trades the lottery for a trickle.

That expected value is small at home scale. Averaged over time, a device
earns about `144 × 3.125 BTC × your hashrate ÷ network hashrate` per day,
plus a share of fees. For a 1.2 TH/s Bitaxe that is roughly 57 sats a day.
Put that next to your electricity rate and the price of the device before
you buy. For most people, at most power prices, buying bitcoin directly costs
less than earning it by mining at home. People mine at home for other
reasons. If you believe in Bitcoin, it is one more way to support it, and
[the section below](#why-solo-mining-helps-the-network) explains how.

### The hardware

- **Small devices** (Bitaxe, NerdQAxe++) draw 15 to 100 W, run quietly on a
  desk, and sit comfortably on a home network. They are the easy way in.
- **Full-size ASICs** draw 3 kW or more, are loud enough that most people
  cannot share a room with one, and often need a 240 V circuit. Their heat is
  real, and some people use them as space heaters in winter.
- Hardware wears. Fans fail, chips degrade, and a device that runs hot will
  throttle or stop. The dashboard's
  [degraded](faq.md#a-miner-is-marked-degraded) and
  [rejecting](faq.md#a-miner-is-marked-rejecting) flags exist because this
  happens.

### If you do find a block

The whole reward goes to the address in the coinbase. It cannot be spent
until 100 more blocks are built on top of it (about 17 hours). On rare
occasions another miner finds a block at the same height at nearly the same
moment, and the network keeps only one of them. If yours is the one dropped,
the reward goes with it.

## Why solo mining helps the network

Here is the realistic part. A home miner's hashrate is a rounding error
against 940 EH/s. Adding a Bitaxe does not measurably change how hard the
chain is to attack. The contribution is somewhere else.

The whitepaper's description of the network starts like this:

> 1. New transactions are broadcast to all nodes.
> 2. Each node collects new transactions into a block.
> 3. Each node works on finding a difficult proof-of-work for its block.
>
> Satoshi Nakamoto, [Bitcoin whitepaper, section 5](https://nakamotoinstitute.org/library/bitcoin/)

In that design, whoever does the work also chooses the block. Pools split the
two apart. With pooled mining, the pool operator builds the template and
every miner in the pool hashes on it without seeing its contents. A handful
of large pools build most of the blocks on the network today. If they decide
to leave certain transactions out, or are required to, the miners behind them
have no say. Their one-CPU-one-vote is cast by the pool.

**Hashrate secures the chain. Templates decide what goes in it.** When you
mine through your own pool on your own node, your node builds the template,
the way step 2 describes:

- **Your node chooses the transactions.** Whatever mempool policy your node
  runs is the policy of your blocks. Nobody upstream can add to or strip from
  them.
- **Your node chooses the chain.** You build on the tip your node validated,
  under the rules your node enforces. You are not trusting a pool operator's
  node to have made that call for you.
- **Your block is one more independent producer.** A transaction that the
  large pools refuse still has a path into the chain as long as independent
  miners exist. That path does not depend on any one miner being large. It
  depends on there being many of them.

Running a node already puts you in the part of Bitcoin that enforces the
rules: a block that breaks them is rejected by every node that checks,
however much hashrate built it. Mining on your own template is the next step.
Your node then produces blocks as well as checking them.

That is the case for solo mining at home. It will probably never pay for
itself. What it does is keep a slice of block production out of the hands of
the few, one small miner at a time.

A home miner is alone but not lonely. Your node, your template, and your
reward answer to nobody else, and every other solo miner is doing the same
thing under the same rules. The network is stronger for how many of you
there are.

## Where this pool fits

Satoshi expected mining to become a specialist's job, and said so a few days
after the announcement:

> At first, most users would run network nodes, but as the network grows
> beyond a certain point, it would be left more and more to specialists with
> server farms of specialized hardware. A server farm would only need to have
> one node on the network and the rest of the LAN connects with that one node.
>
> Satoshi Nakamoto, [November 3, 2008](https://satoshi.nakamotoinstitute.org/emails/cryptography/2/)

He was right about the specialists. What he described is a farm with its own
node and its miners on the LAN behind it. A home setup with one node and a
few miners is the same shape at a smaller scale, and solo-pool-rs is the
piece in the middle. It asks your node for a template, hands work to your
miners, checks their shares, and submits a block to your node the moment one
is found. It pays 100% of the reward to the address you configure and takes
no fee.

It does not change your odds. No pool can. What it gives you is a template
you control and a dashboard that shows what your miners are doing.

To get started, see the [README](../README.md#quick-start-docker), then
[point your miners at the pool](../README.md#pointing-your-miners-at-the-pool). For questions about
dashboard warnings or miner behaviour, see the [FAQ](faq.md).
