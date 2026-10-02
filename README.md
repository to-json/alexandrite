for now, the only human prose in this repo is in this file. for ~ever, this file is only human prose.

alexandrite is a weird experiment. I thought I could probably summon an entire usable 
language in approximate parity with golang 2009 for 20 bux. it's gonna be closer to 5 though,
one week of pro plan usage, while doing some other things. go 09 cost around 4 milli.

lol. 

it's also most of what i like about ruby, rust, zig, and haskell, none of what i dislike,
fast enough to use, with an """obvious""" separating boundary between pure and impure
code to simplify testing. it's like the language you invent to explain haskell to a friend
that you actually want to try it, except, without the "learning haskell" part because
it's also the language sitting at the semantic midpoint of the strongly dynamic languages
that defined the 10s. but with aot compilation. 

you can see if something is ~fallible, type-level-optional?, mutagenic!, or a predicate?, via sigils.
that's the set of properties that, when i think "what do i wish i had a letter for, 
when programming," falls out. mutagenic and predicate are unfortunately, for now, by convention.
you can also tell if something is pure at the _language construct_ level, methods are `def`, pure
functions are `pure def`, `fn`, or `ƒ`. for finger cramp reasons, types are [] and errors are <>.

it's rubyish. you can write little magic blocks wherever you please, and use them as iterators.
you can make them values and throw them around. it's the language _approximately_-you has always-known,
even if _you_ don't know it, today.

i used the extruders to get it because i simply was never going to find the time to write it, and the
environmental impact of extruding it is approximately the same as driving an ev to texas, or flying to 
chicago, from minnesota. that's significant, but, less than just the calories to write it by hand, if
i had. it's a bit like glitching it into being, which is nice because there's code i'd like to write
but i lowk find writing anything else annoying. 

i'll probably burn another flight in tokens optimizing and securing it, and then go 1.0. i really
liked what go did in terms of, like

"this is what it does. there might be a successor, but this one, does what it does". 

this is in that general vicinity. if you've done that method.chaining(oo.shit) before,
and you read the above, about sigil syntax, you know alexandrite. we have current-go's stdlib, or
we will before i stop generating code. current-go's stdlib is expansive enough that you really
don't typically need deps. so, conceptually, alexandrite is ready for that same range of tasks.

i want it because i don't really understand why shell scripting need continue to be a thing,
but i don't want to make my systems dependent on some additional sluggish interpreter. i also
Just Don't Like Go. i like it a lot as a medium for collaboration, but not as a language.

i'm not sure i'll like collaborating in alexandrite tbh, i might prefer go for that. we'll see.

anyway. this gives me the shell scripting business but i can ship binaries. i'm gonna eat a few liberally
licensed golang and ruby faves into the mega-stdlib, and then just, use it. it'll be the language 
my executable pseudocode lives in. you're welcome to do shit with it too. i wouldn't, maybe, especially 
not before 1.0. but you could.

anyway i'm mostly writing this bc i need to publish some alexandrite code. i don't really suggest reading
the prose in here, bc it's changing a lot and is neuralese. i'll update this canary when i can.

byyyyeeeee

