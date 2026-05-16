# OpenOMS



**Issues:**
When an order was not updated asynchronuously throught the 
alpaca reconciliation task it should indicate in the message when 
the order was changed on the brokers side. Otherwise one might think 
that the order is idle until the reconsilation works... 

need to think about it ... because this could also be a desirable feature
because in a sense the order is idle from the perspective of the oms.



**TODO:**



**1) Order Groups**
I have decided that the client already gets the order objects 
for each leg of a composite trade. Therefore, it should be possible
to leave the trade leg management up to the client.
We could use the causation id (which meant for like trading strategies etc.) 
to identify correlated orders. However, I think by using a group id or thinking
about how orders relate to each other would also build the pressure to 
add endpoints to handle group ids, like cancellation, lifecycle management.

**2) Instrument Mappings**
- add broker/exchange <-> master instrument mapping 
    * this also means that there is some sort of interface such that users 
      can add new instrument mappings
- rethink whether the authentication should be based on principal or on account
- should we add an omnibus account? <= well we kinda have that already since we are using one 
  base account for the external broker connection/api
